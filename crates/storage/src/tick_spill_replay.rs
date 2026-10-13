//! Automatic drain for the live-tick spill tier.
//!
//! # Why this exists
//!
//! `tick_persistence` rescues a failed ILP flush by writing the buffer to
//! `data/spill/ticks/ticks-{feed}-{hour}.ilp` instead of discarding it. That
//! converts a permanent in-memory loss into a replayable file — and then
//! stops, because the documented recovery is a `curl` a human has to run. A
//! rescue whose recovery step needs a person fails the standing
//! no-manual-intervention rule, and it fails it SILENTLY: the file sits on
//! disk looking exactly like success.
//!
//! This is a reinstatement, not an invention. A `tick_spill_drain` module
//! existed and was deleted in the 2026-07-17 stage-2 sweep along with the rest
//! of the dead Dhan tick chain (recorded in `lib.rs`). The rescue came back;
//! its drain did not.
//!
//! # Why the file IS the recovery
//!
//! `Buffer::as_bytes()` is InfluxDB line protocol — byte-for-byte the body
//! QuestDB's `/write` endpoint accepts. So there is no bespoke archive format
//! and no parser to keep in sync: replay is POSTing the bytes back.
//!
//! # Why replay is safe to repeat
//!
//! The `ticks` DEDUP key carries `capture_seq`, so a row written twice
//! produces identical keys and the second write UPSERTs onto the first. That
//! is what makes crash-safety free here: a crash between a successful POST and
//! the truncate simply re-POSTs next round. There is no offset file and no
//! bookkeeping state that can drift out of sync with the data.
//!
//! # Why success TRUNCATES rather than deletes
//!
//! `seal_spill::prune_spill_files` distinguishes an aged-out EMPTY file from
//! an aged-out file that still HELD records, and pages AGGREGATOR-DROP-01
//! (`source = "spill_retention"`; until audit PR40b it logged the
//! unregistered `SPILL-RETENTION-01`, which paged nobody) on
//! the latter with "the replay path has been broken for longer than the
//! retention window". A drained file must therefore end up empty, not absent,
//! or that distinction silently stops working.

use std::path::{Path, PathBuf};

use reqwest::Client;
use tickvault_common::error_code::ErrorCode;
use tickvault_trading::candles::multi_tf_aggregator::{
    MAX_PLAUSIBLE_EXCHANGE_TS_SECS, MIN_PLAUSIBLE_EXCHANGE_TS_SECS,
};
use tracing::{error, info, warn};

/// Nanoseconds in one second. Local because `session_window`'s copy is private
/// and this is the only arithmetic in this module that needs it.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Maximum bytes per `/write` POST.
///
/// A spill file may reach `tick_persistence::tick_spill_max_bytes()` (a fraction
/// of the volume; at least 512 MiB),
/// and posting that as one body is precisely the write pressure that caused
/// the spill in the first place. 8 MiB is large enough that a normal rescue is
/// one or two requests and small enough that a full-cap file is paced across
/// many.
pub const REPLAY_MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Bytes of one spill file held in RAM at a time while it is streamed out.
///
/// # Why this exists
///
/// Until 2026-09-03 this module did `std::fs::read(&path)` — it materialised
/// the WHOLE file before chunking it. That is fine for the half-gigabyte the
/// old doc comment assumed, and catastrophic for what the tier actually
/// permits: `spill_failed_ilp` appends to ONE file per feed per clock-hour,
/// and the size rail is a SOFT ceiling on the DIRECTORY which
/// `SpillCeilingVerdict::OverCeilingWithRoom` deliberately allows growth past
/// whenever free space is above the database reserve. There is no per-FILE cap
/// anywhere.
///
/// MEASURED ON PRODUCTION 2026-09-03: a 21 GB depth spill file put process RSS
/// at 20,964,align_bytes -- 20.96 GiB. `MemoryHigh` then throttled the process
/// so hard it could not deliver its 30-second systemd watchdog ping inside
/// `WatchdogSec=60` (raised to 150 the same day), and systemd SIGABRTed a
/// process that was alive and
/// working, every ~9 minutes. Moving that one file out of the drain's path
/// dropped RSS to 0.95 GiB and the kill loop stopped. 22x, from one `read`.
///
/// # Why 32 MiB
///
/// Four times `REPLAY_MAX_CHUNK_BYTES`, so one fill still yields four
/// full-size POSTs and the 8 MiB pacing is untouched: this changes how much is
/// READ, never how much is SENT. A `market_depth` line is ~180 bytes, so this
/// holds ~186,000 of them. Against the app's measured 0.29-1.54 GiB session
/// working set it is noise; against 21 GB it is the whole fix.
pub const REPLAY_STREAM_BUFFER_BYTES: usize = 4 * REPLAY_MAX_CHUNK_BYTES;

const _: () = assert!(
    REPLAY_STREAM_BUFFER_BYTES >= REPLAY_MAX_CHUNK_BYTES,
    "the read buffer must hold at least one full POST body, or `ilp_chunk_ranges` \
     can never emit a full-size chunk and the pacing quietly becomes something else"
);

/// Extension of a replayable spill file. Anything else in the directory is
/// ignored rather than guessed at.
pub const SPILL_FILE_EXTENSION: &str = "ilp";

/// What one replay round did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpillReplayOutcome {
    /// Files whose rows QuestDB accepted this round. A closed file is emptied
    /// only once those acknowledgements are confirmed (audit DA-13b), so it
    /// may still hold its bytes; see `files_awaiting_confirm`. A file whose
    /// round only set bytes aside or refused them as out of window sent
    /// nothing and is not counted (2026-10-13 (DA-13b review)).
    pub files_replayed: usize,
    /// Files whose replay failed; left intact for the next round.
    pub files_failed: usize,
    /// Files already empty — left alone for the age-based pruner.
    pub files_skipped_empty: usize,
    /// Files permanently refused and moved aside so the queue keeps moving.
    pub files_quarantined: usize,
    /// Bytes QuestDB accepted this round.
    pub bytes_replayed: u64,
    /// Single lines QuestDB refused inside an otherwise accepted chunk, kept
    /// in `quarantine/<file>.rejected-lines` (PR17).
    pub lines_set_aside: u64,
    /// Closed files fully drained but kept, because their acknowledgements
    /// are not yet confirmed (audit DA-13b: suspect sink, fewer than two
    /// clean probes since, or inside the durability lag).
    pub files_awaiting_confirm: usize,
}

impl SpillReplayOutcome {
    /// Adds another round's counts to this one (the depth directory drains
    /// its `.ilp` and its framed `.dspl` files in one round).
    pub fn absorb(&mut self, other: Self) {
        self.files_replayed = self.files_replayed.saturating_add(other.files_replayed);
        self.files_failed = self.files_failed.saturating_add(other.files_failed);
        self.files_skipped_empty = self
            .files_skipped_empty
            .saturating_add(other.files_skipped_empty);
        self.files_quarantined = self
            .files_quarantined
            .saturating_add(other.files_quarantined);
        self.bytes_replayed = self.bytes_replayed.saturating_add(other.bytes_replayed);
        self.lines_set_aside = self.lines_set_aside.saturating_add(other.lines_set_aside);
        self.files_awaiting_confirm = self
            .files_awaiting_confirm
            .saturating_add(other.files_awaiting_confirm);
    }
}

/// Splits an ILP payload into line-aligned byte ranges, each at most
/// `max_chunk` bytes.
///
/// # Why line alignment is not a nicety
///
/// Line protocol is newline-delimited. A chunk boundary landing mid-line would
/// hand QuestDB half a row and then start the next body with the other half —
/// corrupting two rows rather than splitting one. So the split point is always
/// the last newline at or before the cap.
///
/// # The one case that cannot obey the cap
///
/// A single line longer than `max_chunk` is emitted WHOLE, as an oversized
/// chunk. The alternatives are to split it (corrupt a row) or drop it (the
/// silent loss this whole tier exists to end), and an oversized POST is the
/// only one of the three that keeps the data intact.
///
/// # Guarantee
///
/// The returned ranges are contiguous, non-overlapping, and cover the entire
/// input — so concatenating them reproduces the payload exactly. Pinned by
/// `chunks_concatenate_back_to_the_exact_payload`.
#[must_use]
pub fn ilp_chunk_ranges(payload: &[u8], max_chunk: usize) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    if payload.is_empty() || max_chunk == 0 {
        return ranges;
    }
    let mut start = 0usize;
    while start < payload.len() {
        let remaining = payload.len() - start;
        if remaining <= max_chunk {
            ranges.push(start..payload.len());
            break;
        }
        let window = &payload[start..start + max_chunk];
        // `position` on the reversed window finds the LAST newline in it.
        let end = match window.iter().rposition(|b| *b == b'\n') {
            // +1 keeps the newline with the line it terminates.
            Some(nl) => start + nl + 1,
            // No newline inside the whole window: one line is longer than the
            // cap. Emit it whole rather than corrupt or drop it.
            None => match payload[start..].iter().position(|b| *b == b'\n') {
                Some(nl) => start + nl + 1,
                None => payload.len(),
            },
        };
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// Lists replayable spill files, oldest first.
///
/// Ordering is by filename, which encodes the hour (`ticks-{feed}-{hour}.ilp`),
/// so the oldest backlog drains first. A missing directory is not an error: a
/// box that never spilled has never created one.
#[must_use]
pub fn list_spill_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(SPILL_FILE_EXTENSION))
        })
        .collect();
    files.sort();
    files
}

/// Builds the QuestDB HTTP `/write` URL.
///
/// Kept as a named function rather than an inline `format!` so the one place
/// that decides this is greppable — a replay pointed at the wrong endpoint
/// would fail every round while looking like a QuestDB outage.
#[must_use]
pub fn write_url(host: &str, http_port: u16) -> String {
    format!("http://{host}:{http_port}/write")
}

/// Directory (under the spill dir) where permanently-refused files are set
/// aside so the rest of the backlog can drain.
pub const QUARANTINE_DIR: &str = "quarantine";

/// Counter for files moved aside because QuestDB will never accept them.
pub const REPLAY_QUARANTINED_COUNTER: &str = "tv_tick_spill_replay_quarantined_total";

/// Is this HTTP status a PERMANENT refusal — one that retrying cannot fix?
///
/// The distinction is the whole point. A 5xx or a timeout means QuestDB is
/// struggling, and the existing "stop the round" behaviour is right: pushing
/// a backlog at a struggling server is how a bounded tick loss becomes an
/// unbounded outage. A 4xx means the PAYLOAD is wrong, and no number of
/// rounds will change a malformed byte — so the same behaviour that protects
/// the server in the first case strands every file behind the bad one in the
/// second. That is what happened on 2026-08-25: one torn line in a 401 KB
/// file kept a 512 MB file holding 1,662,318 intact ticks from ever being
/// attempted, for hours.
///
/// 408 and 429 are 4xx by number and TRANSIENT by meaning — request timeout
/// and rate limit both succeed on a later try — so they are deliberately
/// excluded and take the retry path.
#[must_use]
pub fn is_permanent_refusal(status: u16) -> bool {
    (400..500).contains(&status) && status != 408 && status != 429
}

/// Moves a permanently-refused spill file into the quarantine directory.
///
/// Returns the new path on success. QUARANTINE, NEVER DELETE: the file's
/// surviving lines are usually recoverable by hand — on 2026-08-25, 1,292 of
/// 1,293 were — and a rescue tier that destroys what it cannot parse is worse
/// than the loss it exists to prevent.
///
/// A failure to MOVE the file is itself reported and leaves the file where it
/// is; the caller then treats it as a transient failure, because a file that
/// could not be set aside would otherwise be silently skipped and the queue
/// would appear to drain while it did not.
///
/// NEVER OVERWRITES (2026-10-02): spill files are named per feed and hour, so
/// the same name recurs; a plain `rename` onto an already-quarantined file of
/// that name destroyed it. The target is now the first free name of
/// `<name>`, `<name>.1`, `<name>.2`, … The check-then-rename is not atomic,
/// which is safe here because one drain task owns each spill directory.
fn quarantine_spill_file(dir: &Path, path: &Path) -> std::io::Result<std::path::PathBuf> {
    let quarantine = dir.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&quarantine)?;
    let name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unnamed.ilp")); // APPROVED: infallible fallback, no panic on a pathological path
    let target = free_quarantine_path(&quarantine, name)?;
    std::fs::rename(path, &target)?;
    // The name recurs (one file per feed and hour), so an offset left behind
    // would make the NEXT file of this name start part-way in and skip its
    // first bytes, or read as already drained while it is shorter than the
    // stale offset.
    forget_resume_offset(path);
    Ok(target)
}

/// Most numbered alternatives tried before quarantining gives up (the file
/// then stays where it is and the round treats it as a transient failure).
const QUARANTINE_NAME_ATTEMPTS: u32 = 10_000;

/// The first path in `quarantine` named `name` or `name.<n>` that nothing
/// occupies. `symlink_metadata`, so a dangling link also counts as occupied.
/// Cold path: bounded by [`QUARANTINE_NAME_ATTEMPTS`].
fn free_quarantine_path(
    quarantine: &Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::path::PathBuf> {
    let base = quarantine.join(name);
    if std::fs::symlink_metadata(&base).is_err() {
        return Ok(base);
    }
    let stem = name.to_string_lossy();
    // O(1) EXEMPT: begin — cold quarantine path, bounded by QUARANTINE_NAME_ATTEMPTS
    for n in 1..QUARANTINE_NAME_ATTEMPTS {
        let candidate = quarantine.join(format!("{stem}.{n}"));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    // O(1) EXEMPT: end
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free quarantine name left for this spill file",
    ))
}

/// Start of the daily window in which the DEPTH spill is not replayed: 08:00
/// IST, before the box's 08:30 start, so a boot never replays depth spill
/// ahead of the open.
pub const DEPTH_REPLAY_HOLD_START_SECS_OF_DAY_IST: u32 = 8 * 3600;

/// End of that window: the session close, 15:40 IST. From here the depth
/// spill drains as before, while the sockets are quiet.
pub const DEPTH_REPLAY_HOLD_END_SECS_OF_DAY_IST: u32 =
    tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST;

/// Whether the depth spill replay waits at this IST second of the day.
///
/// # Why (2026-10-05, operator: "go woith your recomemndation dude")
///
/// The box's disk is capped at its 156 MB/s baseline once its burst balance
/// runs out, which on 2026-10-05 happened at about 10:00 IST: the balance fell
/// from 90% at 08:30 to 0% at 10:00 with writes peaking near 500 MB/s, and
/// `market_depth` then fell 54,093 WAL transactions behind, with no row of the
/// day visible. Replaying a 7 to 14 GB depth spill from 08:30 spent that burst
/// before the open and kept competing with live depth all session.
///
/// Holding the replay to after the close is a deferral, never a drop: the
/// files stay on disk (the depth spill keeps accepting past its soft cap while
/// the volume has room), and the raw frames are in the WAL. Ticks are not held;
/// only the depth directory is. O(1).
#[must_use]
pub fn depth_replay_held_at(secs_of_day_ist: u32) -> bool {
    (DEPTH_REPLAY_HOLD_START_SECS_OF_DAY_IST..DEPTH_REPLAY_HOLD_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day_ist)
}

/// Whether `dir` is the depth spill directory (as opposed to the tick one).
#[must_use]
pub fn is_depth_spill_dir(dir: &Path) -> bool {
    dir_label(dir) == dir_label(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR))
}

/// Which session window replay applies to a spill directory's lines.
///
/// Added 2026-10-13 (top_volume spill plan item 5) to replace the ad-hoc
/// per-directory check at the filter site. Each answer matches the WRITER
/// that spilled the rows, so replay refuses exactly what the writer would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayWindow {
    /// Exchange-clocked rows (ticks, and any unknown folder): the strict gate.
    Exchange,
    /// Arrival-clocked rows (depth): the gate with its arrival grace tail.
    Arrival,
    /// No filter (top_volume): its writer has no session gate, and the
    /// exchange gate would delete, for example, a Muhurat-evening board
    /// replayed the next day (`muhurat::current()` is per process).
    None,
}

/// When a spill directory's replay waits instead of posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayHold {
    /// Never held (ticks): the rescue tier must drain as soon as it can.
    Never,
    /// Held 08:00–15:40 IST ([`depth_replay_held_at`]); depth.
    InSession,
    /// Held until the `top_volume_<tf>` tables carry their DEDUP key in this
    /// process (an ILP row could otherwise auto-create a keyless table), and
    /// then 08:00–15:40 IST like depth (owner decision Q1, 2026-10-13).
    KeyedThenInSession,
}

/// How one spill directory is replayed: chosen from the directory by
/// [`replay_policy_for_dir`]. Ticks and depth map to exactly the behaviour
/// they had before this type existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayPolicy {
    /// The `dir` label on the replay counters.
    pub label: &'static str,
    /// Session window applied to every line.
    pub window: ReplayWindow,
    /// When the round is held.
    pub hold: ReplayHold,
    /// Whether the round also drains length-framed depth files (`.dspl`).
    pub framed_depth: bool,
    /// A pending file whose hour is older than this many seconds is moved to
    /// `quarantine/` instead of replayed (top_volume, owner decision Q4(b)).
    /// `None` replays every file whatever its age.
    pub max_file_age_secs: Option<u64>,
}

/// The replay policy of `dir`. O(1): one label lookup.
#[must_use]
pub fn replay_policy_for_dir(dir: &Path) -> ReplayPolicy {
    let label = dir_label(dir);
    let (window, hold, framed_depth) = match label {
        "depth" => (ReplayWindow::Arrival, ReplayHold::InSession, true),
        "top_volume" => (ReplayWindow::None, ReplayHold::KeyedThenInSession, false),
        _ => (ReplayWindow::Exchange, ReplayHold::Never, false),
    };
    ReplayPolicy {
        label,
        window,
        hold,
        framed_depth,
        max_file_age_secs: None,
    }
}

/// Whether a round of `hold` waits at `secs_of_day_ist` given whether the
/// top_volume tables are keyed. Pure. O(1).
#[must_use]
pub fn replay_held(hold: ReplayHold, secs_of_day_ist: u32, top_volume_keyed: bool) -> bool {
    match hold {
        ReplayHold::Never => false,
        ReplayHold::InSession => depth_replay_held_at(secs_of_day_ist),
        ReplayHold::KeyedThenInSession => {
            !top_volume_keyed || depth_replay_held_at(secs_of_day_ist)
        }
    }
}

/// Days of slack between a top_volume spill file's age limit and the hot
/// window after which its table's partitions can be archived and dropped.
pub const TOP_VOLUME_REPLAY_AGE_SLACK_DAYS: u32 = 1;

/// The age past which a top_volume spill file is set aside instead of
/// replayed (owner decision Q4(b), 2026-10-13): the shortest hot window the
/// archive applies to any `top_volume_<tf>` table (the same
/// `hot_window_days` the archive calls, so the two cannot disagree) less
/// [`TOP_VOLUME_REPLAY_AGE_SLACK_DAYS`], never below one day.
///
/// # Why
///
/// The archive drops a `top_volume_<tf>` day partition once its oldest row is
/// older than the hot window. Rows are stamped in IST (5.5 h ahead of the
/// wall clock in UTC), so a day partition holding a row written at `t` can be
/// eligible from `t + (hot − 1) days + 5.5 h`. Setting a file aside at
/// `hot − 1` days keeps it at least 5.5 h ahead of that, so a replay never
/// writes into an hour whose cold copy was already taken. The file is kept in
/// `quarantine/` (uploaded cold before any trim), never deleted. Pure. O(1).
///
/// ⚠ CHANGED 2026-10-13 (PR-C review, MEDIUM): when the disk-pressure archive
/// is enabled, the window is the SMALLER of the normal and the pressure
/// window. A pressure pass cuts market data to `pressure_hot_days` (2 by
/// default) and archives top_volume hours by then, so with the normal window
/// alone a file 2 to 14 days old was neither set aside nor protected and the
/// 15:40 replay wrote into hours already dropped. A pressure pass can run at
/// any time and its drops outlive the episode and a restart, so the smaller
/// window applies whenever pressure archival can run, not only during an
/// episode. Cost: with the shipped config (15-day window, pressure on, 2
/// days) a file more than ONE day old is set aside rather than replayed;
/// replay runs every evening, so only a spill QuestDB could not take for a
/// whole day reaches that, and it is kept and copied cold, never deleted.
#[must_use]
pub fn top_volume_replay_max_age_secs(
    cfg: &tickvault_common::config::PartitionRetentionConfig,
) -> u64 {
    let shortest = |c: &tickvault_common::config::PartitionRetentionConfig| {
        crate::top_volume_rank_persistence::SnapshotCadence::ALL
            .iter()
            .map(|t| crate::partition_archive::hot_window_days(t.table_name(), c))
            .min()
            .unwrap_or(crate::partition_archive::MIN_HOT_DAYS)
    };
    let mut hot = shortest(cfg);
    if cfg.pressure_archive_enabled {
        // The same `min(configured, pressure)` the pressure leg applies
        // (`disk_pressure_boot::pressure_config`) to the market-data class
        // `top_volume_<tf>` belongs to.
        let pressure_days = crate::disk_pressure::pressure_hot_days(cfg);
        let pressure = tickvault_common::config::PartitionRetentionConfig {
            market_data_hot_days: cfg.market_data_hot_days.min(pressure_days),
            depth_hot_days: cfg.depth_hot_days.min(pressure_days),
            intraday_hot_days: cfg.intraday_hot_days.min(pressure_days),
            ..cfg.clone()
        };
        hot = hot.min(shortest(&pressure));
    }
    let days = hot.saturating_sub(TOP_VOLUME_REPLAY_AGE_SLACK_DAYS).max(1);
    u64::from(days).saturating_mul(86_400)
}

/// Whether a spill file of hour `file_hour` is past `max_age_secs` at
/// `now_unix_secs`: its hour STARTED more than `max_age_secs` ago. Pure. O(1).
#[must_use]
pub fn spill_file_older_than(file_hour: u64, max_age_secs: u64, now_unix_secs: u64) -> bool {
    file_hour
        .saturating_mul(SPILL_FILE_HOUR_SECS)
        .saturating_add(max_age_secs)
        < now_unix_secs
}

/// Moves every PENDING spill file of `dir` older than `max_age_secs` to
/// `quarantine/` (top_volume, owner decision Q4(b)). A file that is drained,
/// empty, unnamed by hour, or still being written (not closed and quiet) is
/// left alone. Each move is counted on `tv_spill_replay_aged_set_aside_total`
/// and logged once. Returns the files moved.
///
/// # Complexity
/// O(files in `dir`): one listing and one `stat` per file, on the replay
/// task's blocking round only.
pub fn set_aside_aged_spill_files(dir: &Path, max_age_secs: u64, view: &SpillSinkView) -> u64 {
    let label = dir_label(dir);
    let mut moved: u64 = 0;
    for path in list_spill_files(dir) {
        let Some(hour) = spill_file_hour(&path) else {
            continue;
        };
        if !spill_file_older_than(hour, max_age_secs, view.now_unix_secs) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !spill_file_pending(&path, meta.len())
            || !spill_file_closed_and_quiet(&path, &meta, view)
        {
            continue;
        }
        match quarantine_spill_file(dir, &path) {
            Ok(target) => {
                moved = moved.saturating_add(1);
                metrics::counter!("tv_spill_replay_aged_set_aside_total", "dir" => label)
                    .increment(1);
                warn!(
                    code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                    source = "spill_replay_aged_set_aside",
                    dir = label,
                    from = %path.display(),
                    to = %target.display(),
                    max_age_secs,
                    "spill file is older than its table's hot window allows, so it was SET \
                     ASIDE in quarantine instead of replayed: its hour may already be \
                     archived and dropped, and a replay would recreate that hour with only \
                     these rows. Kept on disk and copied to the cold bucket before any trim; \
                     re-ingest by hand only after checking the cold archive of that hour."
                );
            }
            Err(err) => note_spill_file_unreadable(&path, "spill_aged_set_aside_failed", &err),
        }
    }
    moved
}

/// The spill ceiling of the directory `dir`: the depth ceiling for the depth
/// spill directory, the tick ceiling otherwise. The quarantine trim takes its
/// budget from the directory it trims (until 2026-10-02 the depth quarantine
/// was trimmed against the tick ceiling). O(1) after each ceiling's first
/// resolution.
///
/// ⚠ CHANGED 2026-10-13 (top_volume spill plan item 5): the top_volume
/// folder takes its own hard ceiling.
#[must_use]
pub fn spill_max_bytes_for_dir(dir: &Path) -> u64 {
    match dir_label(dir) {
        "depth" => crate::depth_persistence::depth_spill_max_bytes(),
        "top_volume" => crate::top_volume_rank_persistence::top_volume_spill_max_bytes(),
        _ => crate::tick_persistence::tick_spill_max_bytes(),
    }
}

/// Replays every spill file in `dir` into QuestDB.
///
/// # Why a failing round stops the round
///
/// The spill exists because QuestDB was already too slow to accept a flush.
/// Pushing the whole backlog at a struggling server is how a bounded tick loss
/// becomes an unbounded outage of everything sharing that database. The first
/// non-2xx or transport error therefore ends both the file and the round; the
/// next round starts over from the same file, losing nothing.
/// Per-file resume offsets: how many bytes of each spill file are already in
/// the database.
///
/// # Why this REPLACED an in-place compaction
///
/// The current hour's spill file is held open by the writer with
/// `.append(true)`, so it can grow WHILE it is drained. Three answers were
/// tried on 2026-09-03 and only the third is safe:
///
///   1. Truncate the whole file — destroys the appended bytes. Real tick loss.
///   2. Skip the truncate — destroys nothing and NEVER converges: the whole
///      file, drained prefix included, is re-POSTed every 300 s at the
///      database whose slowness caused the spill.
///   3. Compact in place. This SHIPPED TO PRODUCTION, and an adversarial
///      review of the DEPLOYED code found it carries defect 1 one level down:
///      the tail length is snapshotted before the copy, so bytes appended
///      DURING the copy are never read and are then erased by `set_len`. The
///      window is the whole copy loop, not one syscall — WIDER than the bug
///      it replaced.
///
/// In-place compaction cannot be repaired. With a concurrent appender there is
/// no atomic "move the tail and shorten", and any partial failure leaves
/// [moved tail][stale prefix][new appends] — a torn seam that quarantines the
/// rest of the file on the next round.
///
/// So no bytes are moved and no live file is truncated. The drained offset is
/// REMEMBERED and the next round starts there: nothing to destroy, nothing
/// re-sent, and it converges because each round drains only what is new.
///
/// # Honest limit
///
/// In-memory, so a process restart forgets and re-sends from the start. That
/// is idempotent — the `ticks` dedup key carries `capture_seq`, so a replayed
/// row upserts onto itself — and it costs one extra drain of an at-most-hourly
/// file. Persisting it would need a sidecar with its own crash semantics and
/// its own race with the appender.
static RESUME_OFFSETS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, u64>>,
> = std::sync::OnceLock::new();

fn resume_offsets() -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, u64>>
{
    RESUME_OFFSETS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The byte offset this file has already been drained to, or 0.
pub(crate) fn resume_offset_for(path: &Path) -> u64 {
    resume_offsets()
        .lock()
        .map(|m| m.get(path).copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Record how far a file has been drained. MONOTONE: never moves backwards, so
/// a short round cannot un-drain bytes a longer one already delivered.
pub(crate) fn record_resume_offset(path: &Path, offset: u64) {
    if let Ok(mut m) = resume_offsets().lock() {
        let slot = m.entry(path.to_path_buf()).or_insert(0);
        *slot = (*slot).max(offset);
    }
}

/// Forget a file we will never read again, so the map cannot grow without
/// bound as hours roll over. Also forgets its ack stamp (audit DA-13b): the
/// two describe the same drained bytes and must go together, or a new file of
/// the same name would inherit a stamp for bytes it never held.
pub(crate) fn forget_resume_offset(path: &Path) {
    if let Ok(mut m) = resume_offsets().lock() {
        m.remove(path);
    }
    if let Ok(mut m) = ack_stamps().lock() {
        m.remove(path);
    }
}

// ---------------------------------------------------------------------------
// Audit DA-13b (2026-10-13): when a drained spill file may be emptied.
//
// Three defects shared one cause: the replay emptied a file on the strength of
// facts it never checked.
//
// 1. "QuestDB said 2xx, so the rows are in the database." Not while a table is
//    WAL-suspended: it acknowledges and never applies (the 2026-08-25 disk-full
//    shape), and the rescued rows' WAL frames already read as applied, so the
//    spill file is their only local copy. Nor within seconds of the ack: with
//    `cairo.commit.mode=nosync` the rows sit in the page cache, which is why the
//    persisted watermark lags acks by `WATERMARK_DURABILITY_LAG_SECS`.
// 2. "Only the last file by name can still be written to." The writer names
//    files from the wall clock, so a clock step back across an hour (or a step
//    forward that left a future-named file behind) makes it append to a file
//    that is not last.
// 3. "A file drained while it was live will be emptied when its hour closes."
//    It never was: the fully-drained skip ran before the empty-out.
//
// So a closed, fully drained file is emptied only when every ack that drained
// it has stood for the durability lag, at least two clean QuestDB probes ran
// after the last of them (the seal mid-session replay's rule), no suspicion
// episode began since the first of them, and the sink is not suspect now. An
// episode since the first ack rewinds the file so it is POSTed again; the
// DEDUP keys collapse the rows that did land. Every direction keeps the file.
//
// 2026-10-13 (DA-13b review, round 2): once any check sees the acks
// confirmed they are settled for good, so a later run of episodes cannot
// rewind them into an hour the archive already dropped; a line QuestDB
// refuses on a re-send is kept in quarantine again (it may have been
// acknowledged, never applied, on the first pass); and a non-last file is
// closed only once its hour ended at least `SPILL_LIVE_FILE_QUIET_SECS` ago.
// ---------------------------------------------------------------------------

/// Clean QuestDB probes that must run after a spill file's last ack before the
/// file may be emptied. Same value and reason as the seal mid-session replay's
/// `SEAL_REPLAY_ARCHIVE_CONFIRM_PROBES`: one probe can race the ack; the second
/// ran wholly after it.
pub const SPILL_EMPTY_CONFIRM_PROBES: u64 = 2;

/// Seconds a non-last spill file must have been quiet (by mtime) before the
/// replay treats it as closed, and seconds that must have passed since its
/// HOUR ended. The second rule covers a rescue still writing into the previous
/// hour's file just after the rollover: it read the clock in that hour, then
/// listed the folder and probed the disk before it opened the file, so its
/// write can land after the rollover into a file whose last write was long
/// ago (2026-10-13 (DA-13b review); before, only the mtime was checked, and
/// such a file read as closed the moment the hour turned). An mtime ahead of
/// the clock after a backward step reads as live until the clock passes it.
pub const SPILL_LIVE_FILE_QUIET_SECS: u64 = 120;

/// Seconds in one spill-file hour (the writers name files `now / 3_600`).
const SPILL_FILE_HOUR_SECS: u64 = 3_600;

/// Counter: replay rounds held because the database is suspect.
pub const SPILL_REPLAY_HELD_COUNTER: &str = "tv_spill_replay_held_total";
/// Counter: drained closed files kept because their acks are not confirmed.
pub const SPILL_REPLAY_EMPTY_DEFERRED_COUNTER: &str = "tv_spill_replay_empty_deferred_total";
/// Counter: files rewound to be POSTed again after a suspicion episode.
pub const SPILL_REPLAY_REWOUND_COUNTER: &str = "tv_spill_replay_rewound_total";
/// Counter: non-last files treated as live (clock step, recent write).
pub const SPILL_REPLAY_NONLAST_LIVE_COUNTER: &str = "tv_spill_replay_nonlast_kept_live_total";

/// Reasons a drained closed file is kept for a later round.
const EMPTY_DEFER_REASONS: [&str; 3] = ["sink_suspect", "awaiting_clean_probes", "durability_lag"];

/// Counter: torn last lines of a closed spill file set aside, never POSTed
/// (audit DA-13b review). Local `/metrics` only.
pub const SPILL_REPLAY_TORN_TAIL_COUNTER: &str = "tv_spill_replay_torn_tail_set_aside_total";

/// Where the latest suspicion episode began, as the watermark recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeStart {
    /// The episode's number (`suspect_episodes` once it had begun).
    pub episode: u64,
    /// Clean probes that had run when it began.
    pub clean_probes: u64,
    /// Wall clock when it began, Unix seconds.
    pub unix_secs: u64,
}

/// What the replay knows about the database and the clock for one round.
///
/// Taken once per round by the caller ([`SpillSinkView::current`]) so a round
/// judges every file against the same instant, and so tests can drive time and
/// probes without touching the process-wide watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillSinkView {
    /// A QuestDB table is suspended, its apply lag grows, or the probe is blind.
    pub suspect: bool,
    /// Clean probes reported this process.
    pub clean_probes: u64,
    /// Suspicion episodes begun this process.
    pub suspect_episodes: u64,
    /// Where the latest of those episodes began, when known.
    pub episode_start: Option<EpisodeStart>,
    /// Wall clock, Unix seconds.
    pub now_unix_secs: u64,
    /// Read from the process (`current`), so an ack stamps the probe count
    /// and clock AT THE ACK, not at the round's start: a long round's own
    /// probes must not count as having run after its last ack. `false`
    /// for a view a test built, which is stamped as given.
    pub live: bool,
}

impl SpillSinkView {
    /// The view right now, from the process-wide applied watermark and the
    /// wall clock. O(1): a few atomic loads and one clock read.
    ///
    /// The episode count is read BEFORE and AFTER the suspect flag (audit
    /// DA-13b review). The watcher sets the flag and then bumps the count, so
    /// a read that saw "not suspect" while an episode began reads two
    /// different counts, and the view is then marked suspect (the round is
    /// held). Without that, a view could read "not suspect" with the new
    /// count, and acks sent into the suspended table would be stamped with
    /// the episode they belong to and never rewound.
    #[must_use]
    pub fn current() -> Self {
        let wm = crate::wal_applied_watermark::applied_watermark();
        let episodes_before = wm.suspect_episode_count();
        let suspect = wm.is_sink_suspect();
        let clean_probes = wm.clean_probe_count();
        let episode_start = wm
            .last_episode_start()
            .map(|(episode, clean_probes, unix_secs)| EpisodeStart {
                episode,
                clean_probes,
                unix_secs,
            });
        let episodes_after = wm.suspect_episode_count();
        Self {
            suspect: suspect || episodes_before != episodes_after,
            clean_probes,
            suspect_episodes: episodes_before,
            episode_start,
            now_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            live: true,
        }
    }
}

/// Acks of a spill file that are not yet confirmed: the byte range they
/// covered and when they landed.
#[derive(Debug, Clone, Copy)]
struct PendingAcks {
    /// Resume offset where the first unconfirmed ack's bytes began. A rewind
    /// returns here, never further: everything before it is confirmed.
    from: u64,
    /// Resume offset reached by the last unconfirmed ack.
    through: u64,
    /// Suspicion episodes when the FIRST unconfirmed ack landed: an episode
    /// after it may have lied about any of them.
    first_episode: u64,
    /// When the LAST ack landed, and how many clean probes had run by then.
    last_ack_secs: u64,
    last_ack_probes: u64,
}

impl PendingAcks {
    /// `Ok` once these acks are confirmed: the sink is not suspect, at least
    /// [`SPILL_EMPTY_CONFIRM_PROBES`] clean probes ran after the last one and
    /// the durability lag passed, all before any suspicion episode began. An
    /// episode that began and ended between two rounds no longer voids acks
    /// it cannot have touched: the watermark records where the latest one
    /// began, and acks confirmed by then are settled. Only when more than one
    /// episode began since, or the start record is unreadable, does doubt
    /// remain, and doubt re-sends.
    fn confirmed_at(&self, view: &SpillSinkView) -> Result<(), &'static str> {
        if view.suspect {
            return Err(EMPTY_DEFER_REASONS[0]);
        }
        let lag = crate::wal_applied_watermark::WATERMARK_DURABILITY_LAG_SECS;
        let needed_probes = self
            .last_ack_probes
            .saturating_add(SPILL_EMPTY_CONFIRM_PROBES);
        let needed_secs = self.last_ack_secs.saturating_add(lag);
        if view.suspect_episodes != self.first_episode {
            let settled_before_episode = view.suspect_episodes
                == self.first_episode.saturating_add(1)
                && view.episode_start.is_some_and(|start| {
                    start.episode == view.suspect_episodes
                        && start.clean_probes >= needed_probes
                        && start.unix_secs >= needed_secs
                });
            return if settled_before_episode {
                Ok(())
            } else {
                Err(EMPTY_DEFER_REASONS[0])
            };
        }
        if view.clean_probes < needed_probes {
            Err(EMPTY_DEFER_REASONS[1])
        } else if view.now_unix_secs < needed_secs {
            Err(EMPTY_DEFER_REASONS[2])
        } else {
            Ok(())
        }
    }
}

/// Per-file ack bookkeeping beside the resume offset.
#[derive(Debug, Clone, Copy, Default)]
struct FileAckState {
    /// The acks not yet confirmed, if any.
    pending: Option<PendingAcks>,
    /// Bytes below this offset have been dealt with once already (POSTed, set
    /// aside or refused). After a rewind they are POSTed again. Only bytes
    /// that do not change between passes are skipped there: a torn tail and a
    /// damaged stretch of a framed file are judged on the bytes on disk, so
    /// they are not set aside a second time. A line QuestDB refuses on the
    /// re-send IS set aside and counted again (2026-10-13 (DA-13b review)):
    /// the first pass may have had a 2xx for it from a suspended table, so
    /// the re-send is its only chance to be kept.
    resend_through: u64,
}

impl FileAckState {
    /// `Ok` when this file has no unconfirmed acks, else the reason to wait.
    /// The FIRST time the pending acks read as confirmed they are settled
    /// (`pending = None`), wherever that is seen (2026-10-13 (DA-13b
    /// review)): confirmation is sticky. Before, only the replay's rewind
    /// check settled them, at most one round later, so the archive could rely
    /// on "confirmed" and a second suspicion episode before that round
    /// (`confirmed_at` accepts only one episode since the first ack) read the
    /// same acks as doubtful again and rewound bytes into an hour the archive
    /// had already exported and dropped. O(1), under the caller's lock.
    fn settle_if_confirmed(&mut self, view: &SpillSinkView) -> Result<(), &'static str> {
        let Some(pending) = self.pending else {
            return Ok(());
        };
        pending.confirmed_at(view)?;
        self.pending = None;
        Ok(())
    }
}

/// Ack state per spill file. One entry per file with drained bytes not yet
/// emptied; removed with its resume offset, so it is bounded by the files in
/// the spill directories.
static ACK_STAMPS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, FileAckState>>,
> = std::sync::OnceLock::new();

fn ack_stamps()
-> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, FileAckState>> {
    ACK_STAMPS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Records that QuestDB acknowledged the bytes of `path` between resume
/// offsets `from` and `through` at `view`. Joins the file's pending range (the
/// first unconfirmed ack keeps its start and episode) or opens one. The
/// episode baseline is the ROUND's (an episode that began during the round
/// may have lied about this ack, so it must still rewind it), while the clock
/// and probe count are re-read at the ack for a live view (probes that ran
/// during a long round, before the ack, must not count towards its
/// confirmation). A confirmed range is settled the first time any check sees
/// it confirmed (`FileAckState::settle_if_confirmed`: the rewind check every
/// round runs first, the empty-out verdict, the quarantine report and the
/// archive's pending check), and a later ack opens a fresh range. Cold
/// replay path, one lock and one probe.
pub(crate) fn note_spill_ack(path: &Path, view: &SpillSinkView, from: u64, through: u64) {
    let (ack_secs, ack_probes) = if view.live {
        let now = SpillSinkView::current();
        (
            now.now_unix_secs.max(view.now_unix_secs),
            now.clean_probes.max(view.clean_probes),
        )
    } else {
        (view.now_unix_secs, view.clean_probes)
    };
    if let Ok(mut m) = ack_stamps().lock() {
        let state = m.entry(path.to_path_buf()).or_default();
        state.pending = Some(match state.pending {
            Some(prev) => PendingAcks {
                through: prev.through.max(through),
                last_ack_secs: ack_secs,
                last_ack_probes: ack_probes,
                ..prev
            },
            None => PendingAcks {
                from,
                through,
                first_episode: view.suspect_episodes,
                last_ack_secs: ack_secs,
                last_ack_probes: ack_probes,
            },
        });
    }
}

/// Settles `path`'s pending acks once they are confirmed. If instead a
/// suspicion episode began after the first of them and they were not
/// confirmed before it, rewinds the resume offset to where those acks began
/// (never further: the confirmed prefix is not re-sent) and returns `true`.
/// The episode may have acknowledged rows it never applied; the DEDUP keys
/// collapse the ones that did land. Everything below the old offset is marked
/// as dealt with, so the re-send does not set a torn tail or a damaged stretch
/// aside twice (a line QuestDB refuses on the re-send is still set aside; see
/// `FileAckState::resend_through`).
///
/// Order (2026-10-13 (DA-13b review)): the offset is lowered FIRST and the
/// acks are cleared after, in two separate lock holds (never nested). At
/// every instant one of the two then says "pending" (the offset below the
/// length, or the unconfirmed acks), which is what the partition archive's
/// check (`spill_file_pending`) relies on. Before, the acks were cleared
/// first, and an archive check landing between the two steps read a drained
/// file with nothing unconfirmed while its rows were about to be sent again.
pub(crate) fn rewind_after_suspicion(
    path: &Path,
    view: &SpillSinkView,
    dir_label: &'static str,
) -> bool {
    let drained_to = resume_offset_for(path);
    let rewind_to = ack_stamps()
        .lock()
        .map(|mut m| {
            let state = m.get_mut(path)?;
            let pending = state.pending?;
            if pending.confirmed_at(view).is_ok() {
                state.pending = None;
                return None;
            }
            if pending.first_episode == view.suspect_episodes {
                return None;
            }
            // Decided, but the acks stay pending until the offset is lowered.
            Some(pending.from)
        })
        .unwrap_or(None);
    let Some(rewind_to) = rewind_to else {
        return false;
    };
    rewind_resume_offset(path, rewind_to);
    if let Ok(mut m) = ack_stamps().lock()
        && let Some(state) = m.get_mut(path)
    {
        state.pending = None;
        state.resend_through = state.resend_through.max(drained_to);
    }
    metrics::counter!(SPILL_REPLAY_REWOUND_COUNTER, "dir" => dir_label).increment(1);
    warn!(
        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
        source = "spill_rewound_after_suspicion",
        path = %path.display(),
        rewound_from = drained_to,
        rewound_to = rewind_to,
        "a QuestDB suspicion episode began before this spill file's latest \
         acknowledgements were confirmed, so they may not have been applied: the \
         bytes they covered are sent again (rows that did land collapse onto \
         themselves; a row QuestDB refuses this time is kept in quarantine and \
         counted, even if the first pass kept it already)"
    );
    true
}

/// Moves `path`'s resume offset BACK to `offset` (a rewind). The only caller
/// that may lower it; `record_resume_offset` stays monotone.
fn rewind_resume_offset(path: &Path, offset: u64) {
    if let Ok(mut m) = resume_offsets().lock() {
        m.insert(path.to_path_buf(), offset);
    }
}

/// Bytes of `path` below this offset were dealt with once already; a re-send
/// after a rewind does not set a torn tail or a damaged stretch aside again
/// there (a refused line still is; see `FileAckState::resend_through`).
pub(crate) fn resend_through_for(path: &Path) -> u64 {
    ack_stamps()
        .lock()
        .map(|m| m.get(path).map_or(0, |s| s.resend_through))
        .unwrap_or(0)
}

/// Where `path`'s CONFIRMED rows end, given that it was resumed at
/// `resume_from` this round: the start of its unconfirmed acks when it has
/// any, else `resume_from`. Read before a quarantine, which forgets both.
fn confirmed_through_for(path: &Path, view: &SpillSinkView, resume_from: u64) -> u64 {
    ack_stamps()
        .lock()
        .map(|mut m| {
            let Some(state) = m.get_mut(path) else {
                return resume_from;
            };
            match state.settle_if_confirmed(view) {
                Ok(()) => resume_from,
                Err(_) => state
                    .pending
                    .map_or(resume_from, |p| p.from.min(resume_from)),
            }
        })
        .unwrap_or(0)
}

/// What a whole-file quarantine reports about how much of the file reached
/// the database (audit DA-13b review).
struct QuarantineReport {
    /// Rows before this offset are confirmed in the database.
    confirmed_through: u64,
    /// Rows before this offset were acknowledged (confirmed or not).
    acked_through: u64,
    /// The file's length when it was moved, not at the start of the round.
    len_now: u64,
}

impl QuarantineReport {
    /// Bytes NOT confirmed in the database.
    fn unreplayed_bytes(&self) -> u64 {
        self.len_now.saturating_sub(self.confirmed_through)
    }
}

/// Builds the report for a file about to be quarantined: the confirmed and
/// acknowledged boundaries, and the length re-read now, because a live file
/// grows while it is read. Must run before the move, which forgets the
/// resume offset and the acks. Cold path: one lock and one `stat`.
fn quarantine_report(
    path: &Path,
    view: &SpillSinkView,
    resume_from: u64,
    accepted_this_round: u64,
    file_len_at_start: u64,
) -> QuarantineReport {
    let acked_through = resume_from.saturating_add(accepted_this_round);
    let len_now = std::fs::metadata(path)
        .map_or(file_len_at_start, |m| m.len())
        .max(file_len_at_start)
        .max(acked_through);
    QuarantineReport {
        confirmed_through: confirmed_through_for(path, view, resume_from),
        acked_through,
        len_now,
    }
}

/// Whether `path` holds acks QuestDB has not yet confirmed (audit DA-13b
/// review). The hour-granular partition archive counts such a file as
/// pending, because a rewind may still send its rows again into an hour the
/// archive would otherwise export and drop. One lock and one probe.
#[must_use]
pub(crate) fn has_unconfirmed_ack(path: &Path) -> bool {
    has_unconfirmed_ack_at(path, &SpillSinkView::current())
}

/// [`has_unconfirmed_ack`] at a given view. Acks it finds confirmed are
/// settled for good (`FileAckState::settle_if_confirmed`), so once this has
/// returned `false` for a file, no later suspicion episode can rewind those
/// bytes.
fn has_unconfirmed_ack_at(path: &Path, view: &SpillSinkView) -> bool {
    ack_stamps()
        .lock()
        .map(|mut m| {
            m.get_mut(path)
                .is_some_and(|state| state.settle_if_confirmed(view).is_err())
        })
        // A poisoned lock cannot vouch for anything: count it as pending.
        .unwrap_or(true)
}

/// Whether the partition archive must count the spill file `path`, now `len`
/// bytes long, as pending: bytes past its resume offset, or acks not yet
/// confirmed (2026-10-13 (DA-13b review)).
///
/// Reads the offset, then the acks, then the offset again, each under its own
/// lock (never nested), and calls the file drained only when all three agree.
/// The replay changes the pair in two steps: forward, it stamps the ack and
/// then raises the offset; a rewind lowers the offset and then clears the
/// ack. Either way one of the two says "pending" at every instant, but two
/// separate reads can each catch the other's quiet moment: offset-then-ack
/// can miss a rewind, ack-then-offset can miss a forward step. Reading the
/// offset on both sides of the ack catches both, because the ack read then
/// lies between two offset reads. O(1): two map probes and one ack probe.
#[must_use]
pub(crate) fn spill_file_pending(path: &Path, len: u64) -> bool {
    if len > resume_offset_for(path) {
        return true;
    }
    if len > 0 && has_unconfirmed_ack(path) {
        return true;
    }
    len > resume_offset_for(path)
}

/// Whether a fully drained, closed spill file may be emptied now: `Ok` when it
/// has no unconfirmed acks or they are all confirmed, else the reason to wait.
pub(crate) fn drained_file_empty_verdict(
    path: &Path,
    view: &SpillSinkView,
) -> Result<(), &'static str> {
    if view.suspect {
        return Err(EMPTY_DEFER_REASONS[0]);
    }
    match ack_stamps().lock() {
        // Settles the acks the first time they read as confirmed (sticky).
        Ok(mut m) => m
            .get_mut(path)
            .map_or(Ok(()), |state| state.settle_if_confirmed(view)),
        // A poisoned lock cannot vouch for anything: keep the file.
        Err(_) => Err(EMPTY_DEFER_REASONS[1]),
    }
}
/// The hour a spill file's name carries (`<tier>-<feed>-<hour>.<ext>`).
#[must_use]
pub(crate) fn spill_file_hour(path: &Path) -> Option<u64> {
    path.file_stem()?
        .to_str()?
        .rsplit('-')
        .next()?
        .parse::<u64>()
        .ok()
}

/// Whether the spill writer may still append to `path` (audit DA-13b).
///
/// The last file by name always may. Any other file is treated as closed only
/// when the hour in its name is before the current wall-clock hour AND its
/// mtime is at least [`SPILL_LIVE_FILE_QUIET_SECS`] in the past. A name that
/// does not parse or an mtime that cannot be read reads as live: doubt keeps
/// the data. A non-last file kept live is counted, so a clock step shows.
/// Cold replay path: one compare and one `SystemTime` subtraction.
pub(crate) fn spill_file_may_still_be_appended(
    path: &Path,
    meta: &std::fs::Metadata,
    is_last: bool,
    view: &SpillSinkView,
    dir_label: &'static str,
) -> bool {
    if is_last {
        return true;
    }
    let live = !spill_file_closed_and_quiet(path, meta, view);
    if live {
        metrics::counter!(SPILL_REPLAY_NONLAST_LIVE_COUNTER, "dir" => dir_label).increment(1);
    }
    live
}

/// Whether the hour in `path`'s name ended at least
/// [`SPILL_LIVE_FILE_QUIET_SECS`] ago AND the file has been quiet (by mtime)
/// for as long. The writer names files from the current hour, so such a file
/// receives no more appends; a name that does not parse or an unreadable
/// mtime reads as not closed. Also used for the LAST file's final line: a line
/// with no newline in a closed, quiet file is torn for good, not still being
/// written (audit DA-13b review).
///
/// Honest limit: a rescue that stalls for longer than the quiet period
/// between reading the clock and writing can still land in a file this
/// already reads as closed; a truncate by path cannot exclude a concurrent
/// appender, so closing that fully needs a rename-then-verify empty-out.
pub(crate) fn spill_file_closed_and_quiet(
    path: &Path,
    meta: &std::fs::Metadata,
    view: &SpillSinkView,
) -> bool {
    let hour_closed = spill_file_hour(path).is_some_and(|hour| {
        hour.saturating_add(1)
            .saturating_mul(SPILL_FILE_HOUR_SECS)
            .saturating_add(SPILL_LIVE_FILE_QUIET_SECS)
            <= view.now_unix_secs
    });
    let quiet = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|m| {
            m.as_secs().saturating_add(SPILL_LIVE_FILE_QUIET_SECS) <= view.now_unix_secs
        });
    hour_closed && quiet
}

/// What became of a closed, fully drained file this round.
pub(crate) enum EmptyOutcome {
    /// Truncated to zero bytes.
    Emptied,
    /// Kept for a later round; the reason was counted.
    Deferred,
    /// The truncate failed (coded error logged, failure counted).
    Failed,
}

/// Empties a closed, fully drained spill file once its acks are confirmed
/// (see the section comment above), or keeps it and counts why. Truncates
/// rather than deletes so the retention sweep can still tell a drained file
/// from an abandoned one. Cold replay path.
pub(crate) fn empty_drained_closed_file(
    path: &Path,
    view: &SpillSinkView,
    dir_label: &'static str,
) -> EmptyOutcome {
    if let Err(reason) = drained_file_empty_verdict(path, view) {
        metrics::counter!(
            SPILL_REPLAY_EMPTY_DEFERRED_COUNTER,
            "dir" => dir_label,
            "reason" => reason
        )
        .increment(1);
        return EmptyOutcome::Deferred;
    }
    match std::fs::File::create(path) {
        Ok(_) => {
            forget_resume_offset(path);
            EmptyOutcome::Emptied
        }
        Err(err) => {
            // The rows ARE in QuestDB; only the bookkeeping failed. The offset
            // stays, so the next round does not re-send; it retries the empty.
            error!(
                code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                source = "accepted_not_emptied",
                path = %path.display(),
                %err,
                "spilled rows were accepted by the database but the spill file could not \
                 be emptied — the rows are saved; the empty-out is retried next round"
            );
            metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
            EmptyOutcome::Failed
        }
    }
}

/// Counts and logs a spill file the replay could not stat, open, seek or
/// re-check (audit DA-13b, C10). Before, these returned with an uncoded
/// `warn!` and no counter, so a file that stays unreadable blocked every file
/// behind it, every round, invisibly. Cold replay path.
pub(crate) fn note_spill_file_unreadable(path: &Path, source: &'static str, err: &std::io::Error) {
    metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
    error!(
        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
        source,
        path = %path.display(),
        %err,
        "the spill replay could not read a spill file. Nothing is quarantined and \
         nothing is lost: the file is left as it is and retried next round, but the \
         files behind it wait until it can be read. Check the file's owner, mode and \
         the disk."
    );
}

/// A `&'static str` label for the spill directory being replayed.
///
/// Static, not the path itself: a non-literal label VALUE drops
/// `metrics::counter!` to its allocating arm, and this loop runs once per
/// chunk. The label answers the only question worth asking of this counter --
/// were the refused rows ticks or depth -- and an unrecognised directory says
/// so rather than guessing.
fn dir_label(dir: &Path) -> &'static str {
    let p = dir.to_string_lossy();
    if p.ends_with("ticks") {
        "ticks"
    } else if p.ends_with("depth") {
        "depth"
    } else if p.ends_with("top_volume") {
        // 2026-10-13 (top_volume spill plan item 5): read "other" until then.
        "top_volume"
    } else {
        "other"
    }
}

/// Drop the lines of one ILP chunk whose trailing timestamp is outside every
/// open window, and return the survivors.
///
/// # Why this exists — the one path that can still write an ungated row
///
/// The session-window gate lives in the three WRITERS
/// (`TickWriter::append_tick_with_seq`, `DepthWriter::append_row`,
/// `ShadowCandleWriter::append_row`), so every row that enters an ILP buffer
/// today has already passed it. Replay does not go through a writer: it streams
/// bytes off disk and POSTs them to `/write` verbatim. A spill file written by a
/// pre-gate binary therefore lands in `ticks` or `market_depth` unchecked — and
/// the operator's rule is about what is IN the table, not about which code path
/// put it there.
///
/// # Fail OPEN, deliberately, and this is the whole safety argument
///
/// A line is dropped ONLY when it is positively identified as out of window.
/// Anything this parser does not fully understand is KEPT. The two errors are
/// not symmetric: keeping an out-of-window row reproduces today's behaviour,
/// while dropping a row we merely failed to parse destroys captured market data
/// that the rescue tier exists to protect.
///
/// # How a timestamp is identified
///
/// An ILP line is `table,tags fields timestamp`. Every FIELD is `key=value`, so
/// the only trailing token that can be a bare integer is the timestamp — a line
/// with no timestamp at all (QuestDB then stamps server time) ends in something
/// like `quantity=100i`, which contains `=` and is left alone. Both conditions
/// must hold before a line is even a candidate.
///
/// The value is IST epoch nanoseconds, the same quantity the writers gate on:
/// they append with `.at(TimestampNanos::new(row.ts_ist_nanos))`.
///
/// # Complexity
///
/// O(bytes in the chunk), one pass, no per-line allocation. This is the cold
/// replay path — boot and a periodic drain — never the frame drain.
fn retain_lines_in_open_window(chunk: &[u8], is_arrival_clock: bool) -> (Vec<u8>, u64) {
    let mut kept = Vec::with_capacity(chunk.len());
    let mut dropped: u64 = 0;
    for line in chunk.split_inclusive(|b| *b == b'\n') {
        if line_is_positively_out_of_window(line, is_arrival_clock) {
            dropped = dropped.saturating_add(1);
        } else {
            kept.extend_from_slice(line);
        }
    }
    (kept, dropped)
}

/// [`retain_lines_in_open_window`] on the clock `window` names, or no filter
/// at all for [`ReplayWindow::None`] (top_volume, 2026-10-13). O(bytes).
fn retain_lines_for_window(chunk: &[u8], window: ReplayWindow) -> (Vec<u8>, u64) {
    match window {
        ReplayWindow::Exchange => retain_lines_in_open_window(chunk, false),
        ReplayWindow::Arrival => retain_lines_in_open_window(chunk, true),
        // One copy, as the filter's own `kept` buffer is.
        ReplayWindow::None => (chunk.to_vec(), 0),
    }
}

/// True only when the line carries a parseable ILP timestamp that is BOTH a
/// plausible epoch AND outside every open window. Every other shape returns
/// `false` (keep it).
///
/// # Why the plausibility band is load-bearing, not belt-and-braces
///
/// A spill file's tail can be TORN — the writer was appending when the process
/// died, so the last line may end mid-timestamp: `... ltp=1.0 17160237`. That
/// truncated prefix still parses as an `i64`, and as nanoseconds it lands in
/// 1970 — comfortably outside every open window. Judging on the window alone
/// would therefore DELETE a real captured tick because its timestamp was cut
/// short by a crash, which is the exact loss the rescue tier exists to prevent.
///
/// So a value is only judged when its magnitude says it really is an epoch.
/// Anything below the band (torn, zero, a synthetic `1`, a stray integer) is
/// KEPT and POSTed, and QuestDB judges it — the module's pre-existing torn-tail
/// contract, unchanged.
///
/// The band is the same one the aggregator gates ticks on
/// (`MIN_PLAUSIBLE_EXCHANGE_TS_SECS` ..= `MAX_PLAUSIBLE_EXCHANGE_TS_SECS`), so
/// there is ONE definition of "this integer is a real market timestamp" in the
/// workspace rather than a second one invented here.
fn line_is_positively_out_of_window(line: &[u8], is_arrival_clock: bool) -> bool {
    let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
    let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return false;
    }
    // A `feed_aux_packets` row shares the tick buffer and so its spill files
    // (item 45h). It is stamped at RECEIPT and exists precisely to keep the
    // packets the session window refuses, so the window must never drop it.
    if is_feed_aux_line(trimmed) {
        return false;
    }
    let Some(last) = trimmed.rsplit(|b| *b == b' ').next() else {
        return false;
    };
    // A field is `key=value`; only the timestamp is a bare integer.
    if last.is_empty() || last.contains(&b'=') {
        return false;
    }
    let Ok(text) = std::str::from_utf8(last) else {
        return false;
    };
    let Ok(nanos) = text.parse::<i64>() else {
        return false;
    };
    if !nanos_are_a_plausible_epoch(nanos) {
        return false;
    }
    // CLOCK CHOICE -- load-bearing, added 2026-09-06.
    //
    // The depth spill carries ARRIVAL stamps: `market_depth` has no exchange
    // timestamp, so `DepthWriter` appends the receipt instant and gates it with
    // `arrival_row_is_in_an_open_window` (a bounded ARRIVAL_GRACE_TAIL_SECS tail
    // past the close, for the vendor lag measured at p99 46.4s / max 198.7s).
    // The tick spill carries EXCHANGE stamps and gets the strict form.
    //
    // Judging BOTH with the strict form -- which this function did until today --
    // deletes exactly the rows the writer was just taught to accept: a depth book
    // delivered 15:40:00-15:43:59 IST is written, fails its ILP flush, is rescued
    // to the depth spill, and is then DELETED on replay. The file is truncated
    // either way, so those bytes are gone permanently. That is the same defect
    // the writer fix closed, surviving one tier down in the rescue path.
    if is_arrival_clock {
        !tickvault_common::session_window::arrival_row_is_in_an_open_window(nanos)
    } else {
        !tickvault_common::session_window::row_is_in_an_open_window(nanos)
    }
}

/// True when an ILP line names the `feed_aux_packets` table: the measurement
/// is the text before the first unescaped `,` or space. One prefix compare.
fn is_feed_aux_line(line: &[u8]) -> bool {
    let table = crate::feed_aux_persistence::FEED_AUX_PACKETS_TABLE.as_bytes();
    line.strip_prefix(table)
        .is_some_and(|rest| matches!(rest.first(), Some(b',' | b' ')))
}

/// True when `nanos` is large enough to be a real market timestamp rather than
/// a torn prefix, a zero, or a synthetic fixture value.
///
/// Counter for single lines QuestDB refused inside a chunk it otherwise
/// accepts, set aside to `quarantine/<file>.rejected-lines` (PR17). Paged
/// (the `market_data_refused` loss group). Since 2026-10-13 (DA-13b review)
/// it also counts a torn tail set aside (one per tail, `dir="ticks"` or
/// `"depth"`) and a damaged stretch of a framed depth file (one per stretch),
/// and a line refused again on a re-send after a rewind (counted again).
pub const REPLAY_LINES_REJECTED_COUNTER: &str = "tv_tick_spill_replay_lines_rejected_total";

/// Suffix of the file, in the quarantine directory, that keeps the lines
/// QuestDB refused one by one. Never deleted by the replay; the quarantine
/// trim deletes it only once a verified cold copy exists (copy gate).
pub const REJECTED_LINES_SUFFIX: &str = ".rejected-lines";

/// Most lines one refused chunk may lose to line isolation before the whole
/// file is quarantined instead. A torn line is one or two per crash or full
/// disk; dozens mean the refusal is about the table, not a line.
pub const REPLAY_MAX_REJECTED_LINES_PER_CHUNK: usize = 64;

/// Most POSTs one refused chunk may spend finding its bad lines. Bisection
/// finds one bad line in about log2(lines) POSTs (~17 for a full 8 MiB
/// chunk), so this allows the line cap above with room, and bounds the load
/// on a QuestDB that has just refused us.
pub const REPLAY_MAX_ISOLATION_POSTS_PER_CHUNK: usize = 1_024;

/// What line isolation made of a chunk QuestDB permanently refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineIsolation {
    /// Every other line is in QuestDB; these ranges (into the chunk) were
    /// refused one by one and must be set aside.
    Isolated(Vec<std::ops::Range<usize>>),
    /// No line was accepted, or the caps above were reached: quarantine the
    /// whole file, as before PR17.
    WholeFile,
    /// QuestDB stopped answering or answered with a retryable status: stop
    /// the round; the file is retried next round. Lines already accepted are
    /// re-POSTed then, idempotently (the dedup keys carry the row identity).
    Transient,
}

/// Where to split `range` of `bytes` into two non-empty runs of whole lines:
/// the line boundary nearest the middle. `None` when the range holds one line
/// (its only newline, if any, is its last byte).
///
/// O(range) at worst, cold path (line isolation only).
#[must_use]
pub fn split_at_line_boundary(bytes: &[u8], range: std::ops::Range<usize>) -> Option<usize> {
    let start = range.start;
    let end = range.end.min(bytes.len());
    if end <= start.saturating_add(1) {
        return None;
    }
    // The newline search starts one byte before the middle, so a boundary
    // exactly AT the middle (the byte before it is a newline) is found.
    let mid = (start + (end - start) / 2).saturating_sub(1).max(start);
    // O(1) EXEMPT: begin — cold line isolation, bounded by the chunk size.
    // A split point is the byte AFTER a newline, strictly inside the range.
    let after = bytes[mid..end.saturating_sub(1)]
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| mid + i + 1);
    let split = after.or_else(|| {
        bytes[start..mid]
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| start + i + 1)
    });
    // O(1) EXEMPT: end
    split.filter(|s| *s > start && *s < end)
}

/// Finds the lines QuestDB refuses in a chunk it permanently refused whole.
///
/// PR17: before this, one torn line (a crash or a full disk in the middle of
/// a spill write) quarantined the whole file, so every intact row behind it
/// stayed out of the database until an operator salvaged it. Now the chunk is
/// split at line boundaries and each half re-POSTed; a refused half is split
/// again, down to single lines. Accepted halves are in QuestDB; single lines
/// still refused are returned to be set aside. QuestDB stays the judge, so
/// nothing here parses ILP.
///
/// Bounded: at most [`REPLAY_MAX_ISOLATION_POSTS_PER_CHUNK`] POSTs and
/// [`REPLAY_MAX_REJECTED_LINES_PER_CHUNK`] refused lines, past either the
/// whole file is quarantined as before. A chunk with no accepted line is also
/// `WholeFile` (the refusal may be about the table, not a line), unless
/// `others_accepted`: other rows of the same file already reached QuestDB,
/// so the refused lines are set aside and the file keeps draining. Cold path:
/// runs only after a permanent refusal; allocates the range stack and one
/// `Bytes` slice per POST (no copy).
pub(crate) async fn isolate_refused_lines(
    client: &Client,
    url: &str,
    chunk: &bytes::Bytes,
    others_accepted: bool,
) -> LineIsolation {
    let Some(first_split) = split_at_line_boundary(chunk, 0..chunk.len()) else {
        // One line, already refused. When other rows of the same file were
        // accepted (a torn TAIL after good rows, the crash shape), the line
        // is set aside like any other; when nothing was, the refusal may be
        // about the table, so the file is quarantined as before.
        return if others_accepted {
            LineIsolation::Isolated(std::iter::once(0..chunk.len()).collect()) // APPROVED: cold path, one range
        } else {
            LineIsolation::WholeFile
        };
    };
    // Right half pushed first so the left half is POSTed first: rows reach
    // QuestDB roughly in file order (the dedup keys make order irrelevant).
    let mut stack = vec![first_split..chunk.len(), 0..first_split]; // APPROVED: cold path, after a permanent refusal
    let mut rejected: Vec<std::ops::Range<usize>> = Vec::new(); // APPROVED: cold path, at most REPLAY_MAX_REJECTED_LINES_PER_CHUNK
    let mut posts = 0_usize;
    let mut accepted_any = false;
    while let Some(range) = stack.pop() {
        if posts >= REPLAY_MAX_ISOLATION_POSTS_PER_CHUNK {
            return LineIsolation::WholeFile;
        }
        posts = posts.saturating_add(1);
        let body = chunk.slice(range.clone());
        match client.post(url).body(body).send().await {
            Ok(resp) if resp.status().is_success() => accepted_any = true,
            Ok(resp) if is_permanent_refusal(resp.status().as_u16()) => {
                match split_at_line_boundary(chunk, range.clone()) {
                    Some(split) => {
                        stack.push(split..range.end);
                        stack.push(range.start..split);
                    }
                    None => {
                        rejected.push(range);
                        if rejected.len() > REPLAY_MAX_REJECTED_LINES_PER_CHUNK {
                            return LineIsolation::WholeFile;
                        }
                    }
                }
            }
            Ok(_) | Err(_) => return LineIsolation::Transient,
        }
    }
    if accepted_any || others_accepted {
        LineIsolation::Isolated(rejected)
    } else {
        LineIsolation::WholeFile
    }
}

/// Appends the refused lines to `<dir>/quarantine/<file>.rejected-lines` and
/// syncs it, so a line QuestDB will never take is kept on disk, byte for
/// byte, and copied to the cold bucket like every quarantined file. A line
/// with no trailing newline (a torn tail) gets one, so the next set-aside
/// cannot join it. Returns the bytes kept.
///
/// Cold path, one file write per refused chunk.
pub(crate) fn set_aside_rejected_lines(
    dir: &Path,
    spill_path: &Path,
    chunk: &[u8],
    rejected: &[std::ops::Range<usize>],
) -> std::io::Result<u64> {
    use std::io::Write as _;
    let quarantine = dir.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&quarantine)?;
    let name = spill_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("spill");
    let target = quarantine.join(format!("{name}{REJECTED_LINES_SUFFIX}"));
    let mut out: Vec<u8> = Vec::new(); // APPROVED: cold path, after a permanent refusal
    // O(1) EXEMPT: begin — cold, bounded by REPLAY_MAX_REJECTED_LINES_PER_CHUNK lines.
    for range in rejected {
        let line = chunk.get(range.clone()).unwrap_or_default();
        out.extend_from_slice(line);
        if line.last() != Some(&b'\n') {
            out.push(b'\n');
        }
    }
    // O(1) EXEMPT: end
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)?;
    file.write_all(&out)?;
    file.sync_data()?;
    Ok(out.len() as u64)
}

/// O(1): two integer compares against const bounds, no allocation.
fn nanos_are_a_plausible_epoch(nanos: i64) -> bool {
    let secs = nanos.div_euclid(NANOS_PER_SECOND);
    // O(1) EXEMPT: RangeInclusive::contains on a const range is two integer
    // compares, not a scan.
    u32::try_from(secs).is_ok_and(|s| {
        (MIN_PLAUSIBLE_EXCHANGE_TS_SECS..=MAX_PLAUSIBLE_EXCHANGE_TS_SECS).contains(&s)
    })
}

/// Replays every text spill file in `dir` into QuestDB, judging liveness and
/// the empty-out against `view` (audit DA-13b; see the section comment above
/// [`SpillSinkView`]).
pub async fn replay_spill_dir(
    dir: &Path,
    url: &str,
    client: &Client,
    view: &SpillSinkView,
) -> SpillReplayOutcome {
    let mut outcome = SpillReplayOutcome::default();
    // FIRST-SAMPLE BASELINE. The CloudWatch agent computes a counter as the
    // delta between consecutive samples and DROPS the first sample of a series
    // it has never seen. A counter whose first increment IS the event therefore
    // publishes NOTHING on the one round that matters. Registering the series at
    // zero here -- once per round, before any file is opened -- makes the first
    // real refusal a visible delta. Same discipline as the tick and depth drop
    // counters (`register_drop_baseline`), and the same defect class that hid
    // `tv_depth_rows_spilled_total` on 2026-08-28.
    //
    // O(1) and allocation-free: `dir_label` returns `&'static str`, so the
    // metric name and the label value are both literals and `metrics::counter!`
    // stays on its non-allocating arm.
    metrics::counter!(
        "tv_spill_replay_rows_out_of_window_refused_total",
        "dir" => dir_label(dir)
    )
    .increment(0);
    metrics::counter!(REPLAY_LINES_REJECTED_COUNTER, "dir" => dir_label(dir)).increment(0);
    // ONE buffer for the whole round, reused across files: a fixed cost per
    // round rather than per file, and the thing that makes the peak resident
    // size independent of how large any spill file has grown.
    let mut buf = vec![0_u8; REPLAY_STREAM_BUFFER_BYTES];
    // The writer appends to the file for the CURRENT hour, and `list_spill_files`
    // sorts by name (hour). The LAST entry is always treated as live; since
    // audit DA-13b an earlier entry is closed only when its hour has passed AND
    // it has been quiet for `SPILL_LIVE_FILE_QUIET_SECS`, because the writer
    // names files from the wall clock and a clock step makes it append to a
    // file that is not last. A closed file has no appender, so emptying it
    // cannot destroy anything.
    let label = dir_label(dir);
    let files = list_spill_files(dir);
    let last_index = files.len().saturating_sub(1);
    for (file_index, path) in files.into_iter().enumerate() {
        // Size from METADATA, never from a materialised read. This is the
        // 2026-09-03 fix: `std::fs::read` here held a 21 GB file in RAM.
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                note_spill_file_unreadable(&path, "spill_stat_failed", &err);
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        };
        let file_len = meta.len();
        if file_len == 0 {
            forget_resume_offset(&path);
            // Already drained. Leave it alone: truncating it again would
            // refresh its mtime every round and it would never age out.
            outcome.files_skipped_empty = outcome.files_skipped_empty.saturating_add(1);
            continue;
        }
        // After the empty skip, so a just-emptied (fresh mtime) file is not
        // counted as a live non-last file.
        let writer_may_still_append =
            spill_file_may_still_be_appended(&path, &meta, file_index == last_index, view, label);
        // A final line with no newline in a file whose hour has passed and
        // that has been quiet is torn for good (a crash or a full disk cut
        // the append), even when the file is still the last one.
        let tail_torn_for_good = spill_file_closed_and_quiet(&path, &meta, view);
        // A suspicion episode before this file's latest acks were confirmed:
        // they may be lies, so send their bytes again (audit DA-13b).
        rewind_after_suspicion(&path, view, label);
        // Start where the last round finished, not at 0. See RESUME_OFFSETS:
        // this is what makes a growing file converge WITHOUT moving bytes or
        // truncating a file the writer still holds open.
        let resume_from = resume_offset_for(&path).min(file_len);
        // Below this offset a rewound re-send sets nothing aside and counts
        // no refusal again: those were dealt with on the first pass.
        let resend_through = resend_through_for(&path);
        if resume_from == file_len {
            // Fully drained. A file drained while it was live is emptied here
            // once its hour has closed and its acks are confirmed; before
            // audit DA-13b this skip ran first and such a file was never
            // emptied for the life of the process.
            if !writer_may_still_append {
                match empty_drained_closed_file(&path, view, label) {
                    EmptyOutcome::Emptied => {
                        info!(
                            path = %path.display(),
                            "emptied a drained spill file whose acknowledgements are confirmed"
                        );
                    }
                    EmptyOutcome::Deferred => {
                        outcome.files_awaiting_confirm =
                            outcome.files_awaiting_confirm.saturating_add(1);
                    }
                    EmptyOutcome::Failed => {
                        outcome.files_failed = outcome.files_failed.saturating_add(1);
                        return outcome;
                    }
                }
            }
            outcome.files_skipped_empty = outcome.files_skipped_empty.saturating_add(1);
            continue;
        }
        let mut handle = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                note_spill_file_unreadable(&path, "spill_open_failed", &err);
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        };

        if resume_from > 0 {
            use std::io::Seek as _;
            if let Err(err) = handle.seek(std::io::SeekFrom::Start(resume_from)) {
                // Left for the next round rather than re-sent from the start.
                note_spill_file_unreadable(&path, "spill_seek_failed", &err);
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                continue;
            }
        }

        let mut accepted: u64 = 0;
        // Bytes CONSUMED from the file that were deliberately refused by the
        // window filter and therefore never sent. Tracked separately because
        // `accepted` drives the resume offset and so must count every byte we
        // are done with, while `bytes_replayed` reports what actually reached
        // QuestDB. Folding refusals into the success metric would make it
        // claim rows it declined to write -- the class of false-OK this
        // repository has had to withdraw before.
        let mut refused_bytes: u64 = 0;
        let mut failed = false;
        let mut quarantined = false;
        let mut over_long_line = false;
        // At least one chunk of this file got a 2xx this round (audit
        // DA-13b): the ack is stamped so the empty-out can wait for it to be
        // confirmed.
        let mut acked = false;
        // STREAMING WINDOW STATE.
        //
        // `carry` is the partial line at the FRONT of the buffer, kept from
        // the previous fill. It is the entire mechanism that keeps an ILP line
        // whole across a read boundary: a chunk boundary landing mid-line
        // hands QuestDB half a row and starts the next body with the other
        // half, corrupting TWO rows rather than splitting one.
        let mut carry: usize = 0;
        let mut eof = false;
        'file: while !eof || carry > 0 {
            use std::io::Read as _;
            let cap = buf.len();
            let mut filled = carry;
            while !eof && filled < cap {
                match handle.read(&mut buf[filled..cap]) {
                    Ok(0) => eof = true,
                    Ok(n) => filled = filled.saturating_add(n),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        warn!(
                            path = %path.display(),
                            %err,
                            offset = accepted,
                            "tick spill replay could not read a spill file — leaving it for the \
                             next round. Nothing already accepted is lost: a re-POST of the same \
                             bytes reproduces the same dedup key, so it upserts onto itself."
                        );
                        failed = true;
                        break 'file;
                    }
                }
            }
            if filled == 0 {
                break;
            }
            // File offset of `buf[0]`: every byte before it has been dealt
            // with this round.
            let window_at = resume_from.saturating_add(accepted);
            // Cut at the LAST newline in the window. Everything before it is
            // whole lines; everything after is carried to the next fill.
            let mut cut = match buf[..filled].iter().rposition(|b| *b == b'\n') {
                // +1 keeps the newline with the line it terminates, matching
                // `ilp_chunk_ranges`.
                Some(nl) => nl.saturating_add(1),
                // No newline in the FINAL read of a file that grew past the
                // size read at the start of this round, or of one the writer
                // may still append to and that is not yet closed and quiet:
                // that is a line still being written, not a torn one (audit
                // DA-13b). POSTing it would commit the half row (a prefix cut
                // before its timestamp is valid ILP, which QuestDB stamps with
                // its own time) and the next round would resume mid-line and
                // set the other half aside. Leave it unconsumed; the resume
                // offset stays on the last newline and a later round reads
                // the whole line.
                None if eof
                    && (window_at.saturating_add(filled as u64) > file_len
                        || (writer_may_still_append && !tail_torn_for_good)) =>
                {
                    break 'file;
                }
                // No newline in the FINAL read of a closed, quiet file: a tail
                // torn for good by a crash or a full disk mid-append. It is
                // NEVER POSTed (audit DA-13b review): a fragment cut before
                // its timestamp is valid ILP, which QuestDB would stamp with
                // its own time, a fabricated row. It is set aside whole in
                // `<file>.rejected-lines`, counted on the paged rejected-lines
                // counter like any other line kept out of the database
                // (2026-10-13 (DA-13b review); before, only on a local
                // counter), and the file moves past it. If the rescue that
                // wrote it failed, its frames read unapplied and the WAL still
                // holds the true rows; if the write was only stalled and later
                // completed, the row is in neither, which is why it is paged.
                None if eof => {
                    let len = filled as u64;
                    if window_at >= resend_through {
                        match set_aside_rejected_lines(
                            dir,
                            &path,
                            &buf[..filled],
                            std::slice::from_ref(&(0..filled)),
                        ) {
                            Ok(_) => {
                                metrics::counter!(
                                    SPILL_REPLAY_TORN_TAIL_COUNTER,
                                    "dir" => label
                                )
                                .increment(1);
                                metrics::counter!(
                                    REPLAY_LINES_REJECTED_COUNTER,
                                    "dir" => label
                                )
                                .increment(1);
                                outcome.lines_set_aside = outcome.lines_set_aside.saturating_add(1);
                                error!(
                                    code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                                    source = "spill_torn_tail",
                                    path = %path.display(),
                                    offset = window_at,
                                    bytes = len,
                                    "the last line of a closed spill file is cut short (a crash \
                                     or a full disk during the append). It is NOT sent, because \
                                     QuestDB would store a cut row at its own time; it is kept \
                                     in quarantine as <file>.rejected-lines and is not in the \
                                     database from this file. If the rescue that wrote it \
                                     failed, the WAL still holds the rows; if a stalled write \
                                     completed later, this row is in neither, and its bytes \
                                     are kept here."
                                );
                            }
                            Err(err) => {
                                warn!(
                                    path = %path.display(),
                                    %err,
                                    "the torn last line of a spill file could NOT be written to \
                                     quarantine — the file is kept intact and retried next round"
                                );
                                failed = true;
                                break 'file;
                            }
                        }
                    }
                    accepted = accepted.saturating_add(len);
                    refused_bytes = refused_bytes.saturating_add(len);
                    break 'file;
                }
                // No newline in a FULL buffer with more file behind it. At
                // 32 MiB that is not a long line, it is a torn file.
                //
                // ⚠ HONEST DIFFERENCE from the pre-streaming code, corrected
                // 2026-09-03 after a review caught the claim. An earlier
                // version of this comment said POSTing the window was
                // "byte-for-byte what this function did before streaming".
                // It is NOT. `ilp_chunk_ranges` scans FORWARD and emits an
                // over-long line WHOLE in one body; this cuts at the buffer
                // edge and sends the remainder as a separate body — the
                // split-a-row-across-two-POSTs failure the carry mechanism
                // above exists to prevent.
                //
                // FIXED 2026-09-03 (it was previously accepted-and-noted).
                // Splitting is not merely different, it is DANGEROUS: a
                // truncated ILP line that happens to be missing only its
                // trailing timestamp is still syntactically valid, so QuestDB
                // accepts it and stamps its own time. Half a row committed at
                // the wrong timestamp is worse than either a whole row or no
                // row, and the second half then quarantines the file anyway.
                //
                // A 32 MiB single line is not a legitimate ILP row from any
                // writer here (the depth writer's rows are ~120 bytes), so
                // this arm means the file is corrupt. Refuse it: quarantine
                // preserves every byte on disk for salvage and lets the rest
                // of the backlog drain, which is strictly better than
                // committing a fabricated row.
                None => {
                    over_long_line = true;
                    break 'file;
                }
            };
            // A rewound re-send: end this window exactly at `resend_through`
            // (a line boundary, it was a resume offset) so no chunk straddles
            // it, and every chunk is wholly a repeat or wholly new (the
            // repeat flag only labels the log lines; see below).
            if window_at < resend_through {
                let to_mark = usize::try_from(resend_through - window_at).unwrap_or(usize::MAX);
                if to_mark < cut && buf.get(to_mark.wrapping_sub(1)) == Some(&b'\n') {
                    cut = to_mark;
                }
            }
            let carried = filled.saturating_sub(cut);
            for range in ilp_chunk_ranges(&buf[..cut], REPLAY_MAX_CHUNK_BYTES) {
                // A chunk below `resend_through` was dealt with on an earlier
                // pass. Its refusals are STILL set aside and counted
                // (2026-10-13 (DA-13b review)): the rewind exists because the
                // first pass's 2xx may have come from a suspended table, so a
                // line refused now may never have been kept. The flag only
                // tells the log line the row may already be in quarantine.
                // Honest limit: a line refused on both passes is kept twice
                // and counted twice on the paged counter (errs loud, never
                // drops a row).
                let repeat = window_at.saturating_add(range.start as u64) < resend_through;
                let raw = &buf[range];
                // `len` counts the bytes CONSUMED from the file, not the bytes
                // sent: it drives the resume offset, so a refused line must
                // still advance it or the round would re-read the same bytes
                // forever and never converge.
                let len = raw.len() as u64;
                // The depth spill is ARRIVAL-clocked (see the CLOCK CHOICE note in
                // `line_is_positively_out_of_window`); every other spill dir is
                // exchange-clocked. Deciding it HERE, from the dir being drained,
                // is what keeps replay in agreement with the writer that spilled it.
                // ⚠ CHANGED 2026-10-13 (top_volume spill plan item 5): chosen by
                // `replay_policy_for_dir`; the top_volume folder gets no filter.
                let (chunk, refused_lines) =
                    retain_lines_for_window(raw, replay_policy_for_dir(dir).window);
                refused_bytes =
                    refused_bytes.saturating_add(len.saturating_sub(chunk.len() as u64));
                // Counted on a re-send too (2026-10-13 (DA-13b review)): a
                // re-send must never drop a line without a count, whatever
                // the first pass did with it. The verdict depends only on the
                // line and this binary's session windows, so within one
                // process the first pass refused (and counted) the same lines;
                // they are then counted twice (errs loud).
                if refused_lines > 0 {
                    metrics::counter!(
                        "tv_spill_replay_rows_out_of_window_refused_total",
                        "dir" => dir_label(dir)
                    )
                    .increment(refused_lines);
                    warn!(
                        code = ErrorCode::StorageGapTickDedupSegment.code_str(),
                        refused_lines,
                        resent = repeat,
                        path = %path.display(),
                        "spill replay REFUSED lines whose ILP timestamp is a plausible epoch \
                         OUTSIDE every open window. These are rows a pre-gate binary spilled; \
                         the gate now lives in the writers, so anything spilled after \
                         2026-09-05 is already in window. The refusal is deliberate and is NOT \
                         tick loss: the row would have been refused at the writer too -- \
                         judged on the SAME clock the writer used, arrival for the depth \
                         spill and exchange for the rest. Note \
                         the file is still DRAINED -- a fully-refused closed file is truncated \
                         like any other, because leaving it would re-refuse the same bytes \
                         every round forever. A torn or unparseable stamp is never judged; it \
                         is POSTed and QuestDB decides."
                    );
                }
                if chunk.is_empty() {
                    // Every line in this chunk was out of window. Nothing to
                    // POST, but the bytes are consumed: count them accepted so
                    // the resume offset advances past them.
                    accepted = accepted.saturating_add(len);
                    continue;
                }
                // `Bytes` so line isolation below can re-POST slices of the
                // same chunk without copying it (a refcount clone, O(1)).
                let chunk = bytes::Bytes::from(chunk);
                let sent = chunk.clone();
                match client.post(url).body(chunk).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        accepted = accepted.saturating_add(len);
                        acked = true;
                    }
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        if is_permanent_refusal(status) {
                            // PR17: a torn or malformed LINE must not cost the
                            // whole file. Find the refused lines, keep them in
                            // quarantine, and carry on with the rest; only a
                            // refusal no single line explains falls through
                            // to quarantining the file.
                            match isolate_refused_lines(
                                client,
                                url,
                                &sent,
                                resume_from > 0 || accepted > refused_bytes,
                            )
                            .await
                            {
                                LineIsolation::Isolated(rejected) => {
                                    let rejected_bytes: u64 =
                                        rejected.iter().map(|r| r.len() as u64).sum();
                                    // Kept and counted on a re-send too (see
                                    // `repeat` above): a duplicate line in
                                    // quarantine is harmless, a dropped row
                                    // is not.
                                    match set_aside_rejected_lines(dir, &path, &sent, &rejected) {
                                        Ok(_) => {
                                            let lines = rejected.len() as u64;
                                            metrics::counter!(
                                                REPLAY_LINES_REJECTED_COUNTER,
                                                "dir" => dir_label(dir)
                                            )
                                            .increment(lines);
                                            outcome.lines_set_aside =
                                                outcome.lines_set_aside.saturating_add(lines);
                                            error!(
                                                code = ErrorCode::TickSpill01FileQuarantined
                                                    .code_str(),
                                                path = %path.display(),
                                                status,
                                                lines,
                                                bytes = rejected_bytes,
                                                resent = repeat,
                                                "QuestDB refused single lines in a spill chunk \
                                                 (a torn or malformed row). They are kept in \
                                                 quarantine as <file>.rejected-lines and are \
                                                 NOT in the database; every other row of the \
                                                 chunk was accepted and the drain carries on \
                                                 with the rest of the file. resent=true: the \
                                                 chunk was sent again after a QuestDB \
                                                 suspicion episode, so a line may be kept \
                                                 twice."
                                            );
                                            refused_bytes =
                                                refused_bytes.saturating_add(rejected_bytes);
                                            accepted = accepted.saturating_add(len);
                                            acked = true;
                                            continue;
                                        }
                                        Err(err) => {
                                            warn!(
                                                path = %path.display(),
                                                status,
                                                %err,
                                                "spill lines QuestDB refused could NOT be \
                                                 written to quarantine — the file is kept \
                                                 intact and retried next round. The rows \
                                                 already accepted are re-POSTed then, \
                                                 idempotently."
                                            );
                                            failed = true;
                                            break 'file;
                                        }
                                    }
                                }
                                LineIsolation::Transient => {
                                    warn!(
                                        path = %path.display(),
                                        status,
                                        "QuestDB stopped answering while the refused lines of \
                                         a spill chunk were being isolated — the file is kept \
                                         intact and retried next round"
                                    );
                                    failed = true;
                                    break 'file;
                                }
                                LineIsolation::WholeFile => {}
                            }
                            // The payload is wrong, not the server. Retrying can
                            // never change a malformed byte, and the round-stops-
                            // on-failure rule below would strand every file behind
                            // this one — which is exactly what stranded 1,662,318
                            // intact ticks on 2026-08-25. Set it aside and keep
                            // going.
                            // Audit DA-13b (C11, and its review): the chunks
                            // before this one were acknowledged, but only the
                            // rows before `confirmed_through` are CONFIRMED in
                            // the database; acks after it (this round's, and
                            // any not yet past two clean probes and the
                            // durability lag) can come from a suspended table
                            // that never applies them. Read before the move,
                            // which forgets the offset and the acks, and with
                            // the length as it is NOW (a live file grows
                            // while it is read).
                            let report =
                                quarantine_report(&path, view, resume_from, accepted, file_len);
                            let landed_this_round = accepted.saturating_sub(refused_bytes);
                            match quarantine_spill_file(dir, &path) {
                                Ok(moved) => {
                                    metrics::counter!(REPLAY_QUARANTINED_COUNTER).increment(1);
                                    outcome.files_quarantined =
                                        outcome.files_quarantined.saturating_add(1);
                                    outcome.bytes_replayed =
                                        outcome.bytes_replayed.saturating_add(landed_this_round);
                                    metrics::counter!("tv_tick_spill_replayed_bytes_total")
                                        .increment(landed_this_round);
                                    error!(
                                        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                                        path = %moved.display(),
                                        status,
                                        bytes = report.len_now,
                                        confirmed_through = report.confirmed_through,
                                        acked_through = report.acked_through,
                                        unreplayed_bytes = report.unreplayed_bytes(),
                                        "tick spill file PERMANENTLY refused by QuestDB and moved to \
                                         quarantine so the rest of the backlog can drain. Rows before \
                                         byte confirmed_through are confirmed in the database. Rows \
                                         from confirmed_through to acked_through were acknowledged \
                                         but not yet confirmed (a suspended table acknowledges \
                                         without applying), so treat them as NOT in the database, \
                                         like the rest of the file. Most of the file is usually \
                                         salvageable — filter to well-formed lines and re-POST from \
                                         confirmed_through, which is safe to repeat because the \
                                         ticks dedup key carries capture_seq."
                                    );
                                    // Deliberately NOT `failed = true`: this file
                                    // is dealt with, and the whole point is that
                                    // the queue keeps moving.
                                    quarantined = true;
                                    break 'file;
                                }
                                Err(err) => {
                                    // Could not set it aside. Treat as transient
                                    // rather than skipping it — a file that stays
                                    // in place while being reported as handled
                                    // would make the queue look drained when it
                                    // is not.
                                    warn!(
                                        path = %path.display(),
                                        status,
                                        %err,
                                        "tick spill file was permanently refused but could NOT be \
                                         moved to quarantine — left in place and retried, so the \
                                         backlog behind it is still blocked"
                                    );
                                    failed = true;
                                    break 'file;
                                }
                            }
                        }
                        warn!(
                            path = %path.display(),
                            status,
                            "tick spill replay was refused by QuestDB — the file is kept intact and \
                             retried next round"
                        );
                        failed = true;
                        break 'file;
                    }
                    Err(err) => {
                        warn!(
                            path = %path.display(),
                            error_kind = %describe_send_error(&err),
                            "tick spill replay could not reach QuestDB — the file is kept intact and \
                             retried next round"
                        );
                        failed = true;
                        break 'file;
                    }
                }
            }
            // Carry the partial tail to the front for the next fill.
            carry = carried;
            if carry > 0 {
                buf.copy_within(cut..filled, 0);
            }
        }

        if over_long_line {
            // A line longer than the whole read window means a corrupt file,
            // not a long row. Set it aside INTACT rather than sending half of
            // it: quarantine preserves every byte for salvage and lets the
            // backlog behind it drain, while a split row can be silently
            // accepted with a server-assigned timestamp.
            // Same accounting as the whole-file refusal above (audit DA-13b
            // review): read before the move, which forgets the offset and
            // the acks.
            let report = quarantine_report(&path, view, resume_from, accepted, file_len);
            let landed_this_round = accepted.saturating_sub(refused_bytes);
            match quarantine_spill_file(dir, &path) {
                Ok(moved) => {
                    metrics::counter!(REPLAY_QUARANTINED_COUNTER).increment(1);
                    outcome.files_quarantined = outcome.files_quarantined.saturating_add(1);
                    outcome.bytes_replayed =
                        outcome.bytes_replayed.saturating_add(landed_this_round);
                    metrics::counter!("tv_tick_spill_replayed_bytes_total")
                        .increment(landed_this_round);
                    error!(
                        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                        path = %moved.display(),
                        bytes = report.len_now,
                        confirmed_through = report.confirmed_through,
                        acked_through = report.acked_through,
                        unreplayed_bytes = report.unreplayed_bytes(),
                        window = REPLAY_STREAM_BUFFER_BYTES,
                        "tick spill file contains a line longer than the entire read window, \
                         which no writer here produces — the file is corrupt. Moved to \
                         quarantine with every byte intact rather than POSTing a partial row, \
                         which QuestDB can accept with its own timestamp. Rows before byte \
                         confirmed_through are confirmed in the database; rows from \
                         confirmed_through to acked_through were acknowledged but not yet \
                         confirmed, so treat them, and the rest of the file, as NOT in the \
                         database. Salvage by filtering to well-formed lines and re-POSTing \
                         from confirmed_through, which is safe to repeat because the ticks \
                         dedup key carries capture_seq."
                    );
                    continue;
                }
                Err(err) => {
                    error!(
                        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                        path = %path.display(),
                        %err,
                        "a corrupt tick spill file could not be moved aside — stopping the \
                         round rather than reporting a queue as drained while it is not"
                    );
                    outcome.files_failed = outcome.files_failed.saturating_add(1);
                    metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
                    return outcome;
                }
            }
        }

        if quarantined {
            // Moved aside, not drained. Continue to the NEXT file — the whole
            // reason this branch exists is that the backlog behind a poison
            // file is usually intact and just needs to be attempted.
            continue;
        }

        // Audit DA-13b: stamp the acks of this file so its empty-out waits
        // until they are confirmed, and so a suspicion episode after them
        // rewinds it.
        if acked {
            note_spill_ack(
                &path,
                view,
                resume_from,
                resume_from.saturating_add(accepted),
            );
        }
        // Where this round stopped, always on a line boundary: every chunk is
        // cut at a newline, and a half-written last line of a live file is
        // left unconsumed. Before audit DA-13b the grown and live branches
        // recorded the `file_len` snapshot instead, which can fall mid-line
        // while the writer is appending, so the next round started inside a
        // row.
        let drained_to = resume_from.saturating_add(accepted);
        // Bytes QuestDB actually took this round. `accepted` also counts bytes
        // set aside (a torn tail, refused lines) or refused by the window
        // filter, so a round with `accepted > 0` may have sent nothing; it is
        // then no recovery and is not reported as one (2026-10-13 (DA-13b
        // review)).
        let landed = accepted.saturating_sub(refused_bytes);
        if failed {
            // Keep what this round finished. `accepted` counts only chunks
            // that were fully handled (POSTed and accepted, refused as out of
            // window, or their refused lines written to quarantine), each one
            // cut on a line boundary, so `resume_from + accepted` is the start
            // of the first chunk not handled. Before 2026-10-04 a failure
            // later in the file dropped this, the next round restarted at the
            // old offset, and every line already set aside was isolated and
            // appended to `<file>.rejected-lines` again, once per retry (audit
            // N1). Re-POSTing the accepted rows was harmless (they upsert onto
            // themselves); the duplicate quarantine lines and the repeated
            // bisection work were not.
            if accepted > 0 {
                record_resume_offset(&path, drained_to);
                outcome.bytes_replayed = outcome
                    .bytes_replayed
                    .saturating_add(accepted.saturating_sub(refused_bytes));
                metrics::counter!("tv_tick_spill_replayed_bytes_total")
                    .increment(accepted.saturating_sub(refused_bytes));
            }
            outcome.files_failed = outcome.files_failed.saturating_add(1);
            metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
            // Stop the whole round: QuestDB is unhappy and the rest of the
            // backlog would only make it unhappier.
            return outcome;
        }

        // GROWTH CHECK -- added 2026-09-03 with the streaming rewrite.
        //
        // `list_spill_files` does NOT exclude the file the writer is currently
        // appending to: `spill_failed_ilp` opens `<tier>-<feed>-<hour>.ilp`
        // with `.append(true)`, so the CURRENT hour's file is live while we
        // drain it. `File::create` below truncates the shared inode to zero,
        // which would destroy any bytes appended after our last read.
        //
        // This was already a bug with the whole-file `read` -- the window was
        // one syscall. Streaming widens that window from a syscall to minutes
        // on a large file, so the check lands with the fix rather than after
        // it. Cost is one `stat`. The failure direction is deliberately safe:
        // a spurious skip costs one extra round of re-POSTs, which are free
        // because the dedup key makes them upsert onto themselves; a missed
        // skip costs real market data.
        match std::fs::metadata(&path).map(|meta| meta.len()) {
            Ok(len_now) if len_now > file_len => {
                outcome.bytes_replayed = outcome
                    .bytes_replayed
                    .saturating_add(accepted.saturating_sub(refused_bytes));
                metrics::counter!("tv_tick_spill_replayed_bytes_total")
                    .increment(accepted.saturating_sub(refused_bytes));

                // The writer appended while we drained. Both obvious answers
                // are wrong, and the first version of this shipped one of them.
                //
                //   * Truncate the whole file: destroys the appended bytes.
                //     That is the pre-2026-09-03 behaviour and it is real tick
                //     loss.
                //   * Skip the truncate and retry next round: destroys nothing
                //     and NEVER CONVERGES. `list_spill_files` includes the
                //     CURRENT hour's file, which is open with `.append(true)`,
                //     so during an ongoing spill episode it grows every round
                //     — and the whole file, drained prefix included, is
                //     re-POSTed every 300 s at the database whose slowness
                //     caused the spill. At 21 GB that is the module header's
                //     own warning ("pushing the whole backlog at a struggling
                //     server is how a bounded tick loss becomes an unbounded
                //     outage") arriving through the door meant to prevent it.
                //
                // So: drop exactly the drained PREFIX and keep the tail. The
                // copy moves toward the FRONT (dst offset always below src),
                // so a forward chunked copy can never overwrite a source byte
                // it has not read yet, and it is bounded by what was appended
                // during one drain rather than by the file.
                // NOTHING IS MOVED AND NOTHING IS TRUNCATED. The in-place
                // compaction that shipped here was found by an adversarial
                // review of the DEPLOYED code to carry the very defect it
                // replaced — it froze the tail length before copying, so bytes
                // appended DURING the copy were erased by `set_len`. See
                // RESUME_OFFSETS for why that cannot be repaired in place.
                record_resume_offset(&path, drained_to);
                info!(
                    path = %path.display(),
                    len_before = file_len,
                    len_now,
                    resume_from = drained_to,
                    bytes = accepted,
                    "spilled rows were accepted; the file GREW while draining, so it is left \
                     BYTE-FOR-BYTE INTACT and the next round resumes at the drained offset. \
                     Nothing is moved, nothing is truncated, nothing is re-sent."
                );
                if landed > 0 {
                    outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                }
                continue;
            }
            Ok(_) => {}
            Err(err) => {
                // NOT truncating, so no appended bytes can be destroyed. The
                // rows already accepted keep their offset; the next round
                // resumes after them and never re-sends them, so they are
                // counted here like every other exit counts them (2026-10-13
                // (DA-13b review); before, they were never counted).
                if accepted > 0 {
                    record_resume_offset(&path, drained_to);
                    outcome.bytes_replayed = outcome.bytes_replayed.saturating_add(landed);
                    metrics::counter!("tv_tick_spill_replayed_bytes_total").increment(landed);
                }
                note_spill_file_unreadable(&path, "spill_recheck_failed", &err);
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        }

        // Every chunk accepted.
        //
        // ⚠ THE LAST PLACE AN APPEND COULD STILL BE DESTROYED, closed
        // 2026-09-03. `File::create` truncates the shared inode, and the
        // writer can append between the size re-check above and this call —
        // a race the size check narrows and cannot close. An adversarial
        // review of the deployed code named it, and it is the third variant
        // of the same bug found in one day.
        //
        // So the truncate now runs ONLY on a file the writer has provably
        // moved on from: not the last (current-hour) entry. For the live
        // file the drained offset is recorded instead — the same mechanism
        // the grown-file branch uses, with the same guarantee: nothing is
        // touched, so nothing can be lost, and the next round resumes rather
        // than re-sending.
        // Rows accepted this round are counted when they are accepted; the
        // empty-out of a closed file is a separate, later decision.
        record_resume_offset(&path, drained_to);
        outcome.bytes_replayed = outcome
            .bytes_replayed
            .saturating_add(accepted.saturating_sub(refused_bytes));
        metrics::counter!("tv_tick_spill_replayed_bytes_total")
            .increment(accepted.saturating_sub(refused_bytes));
        if writer_may_still_append || drained_to < file_len {
            // Nothing reached QuestDB this round (only a line still being
            // written was found, or only bytes set aside or refused by the
            // window filter): no "recovered" line and no count (audit DA-13b
            // review: it read as a recovery every 300 s).
            if landed == 0 {
                continue;
            }
            outcome.files_replayed = outcome.files_replayed.saturating_add(1);
            info!(
                path = %path.display(),
                bytes = landed,
                resume_from = drained_to,
                "recovered spilled ticks into the database. The writer may still append to \
                 this file (or its last line is still being written), so it is left \
                 BYTE-FOR-BYTE INTACT and the next round resumes at the drained offset — \
                 emptying it here could destroy a concurrent append."
            );
            continue;
        }

        // A closed file: no writer holds it, so truncating is safe once the
        // acks that drained it are confirmed (audit DA-13b). Until then it is
        // kept, and a later round empties it from the fully-drained branch
        // at the top of the loop. Counted and logged as a recovery only when
        // rows reached QuestDB this round.
        if landed > 0 {
            outcome.files_replayed = outcome.files_replayed.saturating_add(1);
        }
        match empty_drained_closed_file(&path, view, label) {
            EmptyOutcome::Emptied if landed > 0 => {
                info!(
                    path = %path.display(),
                    bytes = landed,
                    "recovered spilled ticks into the database — rows a failed flush would \
                     otherwise have lost are now queryable"
                );
            }
            EmptyOutcome::Emptied => {
                info!(
                    path = %path.display(),
                    bytes = accepted,
                    "emptied a drained spill file. Nothing was sent this round: its last \
                     bytes were set aside in quarantine or refused as out of window"
                );
            }
            EmptyOutcome::Deferred => {
                outcome.files_awaiting_confirm = outcome.files_awaiting_confirm.saturating_add(1);
                if landed > 0 {
                    info!(
                        path = %path.display(),
                        bytes = landed,
                        "recovered spilled ticks into the database; the drained file is kept \
                         until QuestDB's acknowledgement is confirmed (two clean probes and \
                         the durability lag), then emptied"
                    );
                } else {
                    info!(
                        path = %path.display(),
                        bytes = accepted,
                        "drained a spill file with nothing sent this round (its last bytes \
                         were set aside or refused as out of window); it is kept until its \
                         earlier acknowledgements are confirmed, then emptied"
                    );
                }
            }
            EmptyOutcome::Failed => {
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        }
    }
    outcome
}

/// Describes a transport failure without echoing the URL.
///
/// `reqwest::Error`'s `Display` embeds the request URL, and a proxy-bearing URL
/// can carry Basic-Auth userinfo — the same hazard `http_client::redact_userinfo`
/// exists for. The endpoint is ours and fixed, so the KIND is the whole
/// diagnostic value.
#[must_use]
pub fn describe_send_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_body() {
        "body"
    } else if err.is_decode() {
        "decode"
    } else {
        "other"
    }
}

/// Pre-registers both replay counters at zero.
///
/// A replay round is rare, so its first increment would otherwise be consumed
/// as the CloudWatch delta baseline and the episode would go unreported — the
/// same reason `tick_persistence` pre-registers its drop and spill counters.
pub fn register_replay_baseline() {
    metrics::counter!("tv_tick_spill_replayed_bytes_total").increment(0);
    metrics::counter!("tv_tick_spill_replay_failed_total").increment(0);
    // Audit DA-13b: the empty-out gate's local counters, every label pair.
    for dir in ["ticks", "depth"] {
        metrics::counter!(SPILL_REPLAY_HELD_COUNTER, "dir" => dir, "reason" => "sink_suspect")
            .increment(0);
        metrics::counter!(SPILL_REPLAY_REWOUND_COUNTER, "dir" => dir).increment(0);
        metrics::counter!(SPILL_REPLAY_NONLAST_LIVE_COUNTER, "dir" => dir).increment(0);
        metrics::counter!(SPILL_REPLAY_TORN_TAIL_COUNTER, "dir" => dir).increment(0);
        for reason in EMPTY_DEFER_REASONS {
            metrics::counter!(SPILL_REPLAY_EMPTY_DEFERRED_COUNTER, "dir" => dir, "reason" => reason)
                .increment(0);
        }
    }
}

/// Seconds between replay rounds.
///
/// Five minutes, not five seconds. A spill only exists because QuestDB was
/// already too slow to accept a flush, so the drain is deliberately unhurried:
/// the rows are safe on disk, and the cost of arriving late is a query that
/// misses them for a few minutes. The cost of arriving too eagerly is adding
/// write pressure to a database that just proved it had none to spare.
pub const REPLAY_INTERVAL_SECS: u64 = 300;

/// Seconds before a died-and-respawned drain tries again.
pub const REPLAY_RESPAWN_BACKOFF_SECS: u64 = 30;

/// Request timeout for one chunk POST.
///
/// Far longer than the 2s readiness probe: this body can be 8 MiB and is being
/// sent to a server whose slowness is the reason the file exists. Timing out
/// early would turn a slow-but-working recovery into a permanent failure loop.
pub const REPLAY_REQUEST_TIMEOUT_SECS: u64 = 60;

/// Runs replay rounds forever.
///
/// Returns only if the HTTP client cannot be built, which the supervisor
/// treats as a respawn-worthy exit.
async fn run_replay_loop(dir: PathBuf, url: String, require_upload: bool, policy: ReplayPolicy) {
    let client = match crate::http_client::build_probe_client(REPLAY_REQUEST_TIMEOUT_SECS) {
        Ok(client) => client,
        Err(err) => {
            error!(
                code = tickvault_common::error_code::ErrorCode::TickFlush01WorkerRespawn.code_str(),
                %err,
                "tick spill drain could not build an HTTP client — spilled ticks will stay on \
                 disk until this is resolved; they are not lost, but they are not queryable"
            );
            return;
        }
    };
    register_replay_baseline();
    // ⚠ CHANGED 2026-10-13 (top_volume spill plan item 5): the hold comes
    // from the directory's `ReplayPolicy`. Depth is held in session exactly as
    // before; the top_volume folder is also held until its tables are keyed.
    let holds_in_session = policy.hold == ReplayHold::InSession;
    if holds_in_session {
        metrics::counter!("tv_depth_spill_replay_held_total").increment(0);
    }
    if policy.hold == ReplayHold::KeyedThenInSession {
        metrics::counter!(SPILL_REPLAY_HELD_COUNTER, "dir" => policy.label, "reason" => "unkeyed")
            .increment(0);
        metrics::counter!(SPILL_REPLAY_HELD_COUNTER, "dir" => policy.label, "reason" => "in_session")
            .increment(0);
        metrics::counter!("tv_spill_replay_aged_set_aside_total", "dir" => policy.label)
            .increment(0);
    }
    // One coded `warn!` per suspicion episode, not one per held round.
    let mut suspect_logged = false;
    // DRAIN FIRST, THEN SLEEP (2026-08-25). This loop slept 300s before its
    // first round, and that ordering cost 1,695,983 ticks on the live box this
    // morning — permanently, in a single event.
    //
    // The sequence, from the box's own log and counters:
    //
    // | 08:31:09 | boot #1's WAL-replay flush fails; 1,774,802 rows are
    //              RESCUED, writing 544,034,728 bytes — one rescue that alone
    //              exceeds the 512 MiB ceiling |
    // | 08:31:56 | the deploy swaps the binary; the process restarts, and this
    //              loop's 300s timer restarts with it |
    // | 08:33:44 | boot #2's flush fails; the rescue is REFUSED, "at or past
    //              its 536870912-byte cap"; 1,695,983 ticks dropped and
    //              `tv_ticks_spilled_total` stays 0 |
    // | ~08:37:12 | this loop finally wakes and drains that 544 MB — two and a
    //              half minutes after the room it would have freed was needed |
    //
    // Note what the backlog actually WAS: not yesterday's leftover, but the
    // first boot's own successful rescue, 2.5 minutes earlier. A restart
    // during boot therefore produces two large rescues in quick succession
    // while this timer is resetting through both of them.
    //
    // Boot is the WORST possible moment to be asleep. The boot WAL replay is
    // the largest single flush the process ever attempts, so it is both the
    // most likely rescue to be needed AND the one most likely to find the
    // directory already occupied — and a deploy restart, which is exactly when
    // boots happen twice, resets this timer each time.
    //
    // The interval is unchanged and its reasoning still holds: a spill exists
    // because QuestDB was already too slow, so steady-state draining stays
    // unhurried. That argument is about the SECOND round onward. It never
    // argued for entering the day with a full dir.
    loop {
        // ⚠ CHANGED 2026-10-03 (O(1) sweep S3): the round runs on the blocking
        // pool. `replay_spill_dir` reads each spill file in 8 MiB chunks with
        // `std::fs`, and the quarantine trim lists and deletes files — both
        // block. On a tokio worker they held a thread that the frame drain and
        // the socket readers also run on, for as long as a slow disk took.
        // The HTTP posts still run on this runtime (`Handle::block_on` drives
        // the future from the blocking thread; the runtime's drivers serve it).
        let round_dir = dir.clone();
        let round_url = url.clone();
        let round_client = client.clone();
        let runtime = tokio::runtime::Handle::current();
        // The depth directory waits for the close (see `depth_replay_held_at`).
        // The quarantine trim below still runs every round.
        let keyed = crate::top_volume_rank_persistence::TOP_VOLUME_TABLES_KEYED
            .load(std::sync::atomic::Ordering::Acquire);
        let held = replay_held(
            policy.hold,
            tickvault_common::market_hours::now_ist_secs_of_day(),
            keyed,
        );
        if held && holds_in_session {
            metrics::counter!("tv_depth_spill_replay_held_total").increment(1);
        }
        if held && policy.hold == ReplayHold::KeyedThenInSession {
            let reason = if keyed { "in_session" } else { "unkeyed" };
            metrics::counter!(SPILL_REPLAY_HELD_COUNTER, "dir" => policy.label, "reason" => reason)
                .increment(1);
        }
        // Audit DA-13b: while a QuestDB table is suspended (or its apply lag
        // grows, or the probe is blind), a 2xx is not proof the rows were
        // applied, so nothing is POSTed and nothing is emptied. Logged once
        // per episode, counted every held round.
        let view = SpillSinkView::current();
        if view.suspect {
            metrics::counter!(
                SPILL_REPLAY_HELD_COUNTER,
                "dir" => dir_label(&dir),
                "reason" => "sink_suspect"
            )
            .increment(1);
            if !suspect_logged {
                suspect_logged = true;
                warn!(
                    code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                    source = "spill_replay_held_sink_suspect",
                    dir = %dir.display(),
                    "spill replay is holding: QuestDB reports a suspended table, a growing \
                     apply lag, or its probe is blind, so an acknowledgement would not prove the \
                     rows were applied. The spill files are kept as they are and the replay \
                     resumes once a probe is clean."
                );
            }
        } else {
            suspect_logged = false;
        }
        let held_suspect = view.suspect;
        let max_file_age_secs = policy.max_file_age_secs;
        let framed_depth = policy.framed_depth;
        let round = tokio::task::spawn_blocking(move || {
            // Owner decision Q4(b) (top_volume, 2026-10-13): a file too old
            // for its table's hot window is set aside BEFORE any replay, on
            // every round, held or not.
            if let Some(max_age) = max_file_age_secs {
                let _set_aside = set_aside_aged_spill_files(&round_dir, max_age, &view);
            }
            let outcome = if held || held_suspect {
                SpillReplayOutcome::default()
            } else {
                let mut outcome = runtime.block_on(replay_spill_dir(
                    &round_dir,
                    &round_url,
                    &round_client,
                    &view,
                ));
                // The depth directory also holds length-framed `.dspl` files
                // (plan item 49e, 2026-10-09). Older `.ilp` files drain first;
                // a failed text round leaves the framed files for the next
                // round, as any failure stops the round.
                if framed_depth && outcome.files_failed == 0 {
                    outcome.absorb(runtime.block_on(
                        crate::depth_spill_frame::replay_framed_spill_dir(
                            &round_dir,
                            &round_url,
                            &round_client,
                            &view,
                        ),
                    ));
                }
                outcome
            };
            // Trim quarantine on every round, not only at boot (2026-08-28,
            // round-2 fix). Quarantine is written BY THIS LOOP during the
            // session — a permanently-refused file is set aside here — so a
            // boot-only trim lets a heavy-quarantine day ratchet past the
            // ceiling mid-morning and re-disable the rescue tier until the
            // next boot, which is the exact failure the trim exists to
            // prevent. Cheap: it reads one flat directory and returns
            // immediately when the bytes are inside budget.
            //
            // 2026-10-02: the budget is THIS directory's: the depth quarantine
            // used to be trimmed against the tick spill ceiling. And a file is
            // deleted only with a verified cold copy (plan item 45e-1).
            let pruned = crate::tick_persistence::prune_quarantine(
                &round_dir,
                spill_max_bytes_for_dir(&round_dir),
                require_upload,
            );
            (outcome, pruned)
        })
        .await;
        // A panicked round ends this loop; the supervisor logs it, counts it
        // and respawns after its backoff, exactly as for a panic here.
        let Ok((outcome, pruned)) = round else {
            return;
        };
        if outcome.files_replayed > 0 || outcome.files_failed > 0 {
            info!(
                files_replayed = outcome.files_replayed,
                files_failed = outcome.files_failed,
                bytes_replayed = outcome.bytes_replayed,
                "tick spill drain round complete"
            );
        }
        if pruned > 0 {
            warn!(
                files = pruned,
                "trimmed the quarantine directory mid-session — see the preceding coded \
                 lines for each file"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(REPLAY_INTERVAL_SECS)).await;
    }
}

/// Spawns the drain under a supervisor that respawns it if it ever exits.
///
/// # Why supervised
///
/// An unsupervised background task that dies takes its whole job with it and
/// says nothing — and this job is the recovery arm of the tick rescue tier, so
/// its silent death would mean spilled ticks accumulate to the 512 MiB cap and
/// are then dropped, with the operator seeing only the drop. Same shape as the
/// disk-health watcher and the pool supervisor.
///
/// `TICK-FLUSH-01` is reused rather than duplicated: its meaning is "the
/// off-thread tick ILP flush worker died and the supervisor respawned it",
/// which is exactly this. Its previous emit site was deleted in the 2026-07-17
/// sweep; the runbook carries a dated note recording this revival.
///
/// `require_upload` is `[raw_frame_archive] require_upload_before_prune`: the
/// per-round quarantine trim then deletes only files with a verified cold
/// copy (plan item 45e-1).
pub fn spawn_supervised_tick_spill_replay(
    dir: PathBuf,
    host: &str,
    http_port: u16,
    require_upload: bool,
) -> tokio::task::JoinHandle<()> {
    let policy = replay_policy_for_dir(&dir);
    spawn_supervised_spill_replay(dir, write_url(host, http_port), require_upload, policy)
}

/// The third supervised replay, for the top_volume spill folder
/// ([`crate::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR`]; top_volume
/// spill plan item 5, 2026-10-13). Same loop as ticks and depth with the
/// top_volume [`ReplayPolicy`]: no window filter, held until the tables are
/// keyed and then 08:00–15:40 IST, and a file older than
/// [`top_volume_replay_max_age_secs`] set aside rather than replayed.
///
/// Trims the folder's quarantine once first, against its own ceiling, as the
/// boot does for ticks (plan item 7). Spawned by the app on every boot, also
/// with `[top_volume] spill_enabled = false`, so files already on disk are
/// replayed and aged and never hold the archive forever (PR-C review).
// TEST-EXEMPT: spawns the supervised loop; the policy it passes is asserted by test_top_volume_replay_holds_until_tables_are_keyed and test_top_volume_spawn_sets_the_age_limit_from_the_archive_window, and the boot call site by the app's top_volume spill wiring test.
pub fn spawn_supervised_top_volume_spill_replay(
    host: &str,
    http_port: u16,
    require_upload: bool,
    retention: &tickvault_common::config::PartitionRetentionConfig,
) -> tokio::task::JoinHandle<()> {
    let dir = PathBuf::from(crate::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR);
    let pruned = crate::tick_persistence::prune_quarantine(
        &dir,
        spill_max_bytes_for_dir(&dir),
        require_upload,
    );
    if pruned > 0 {
        warn!(
            files = pruned,
            "trimmed the top_volume quarantine directory to its share of its spill ceiling \
             — see the preceding coded lines for each file"
        );
    }
    let policy = top_volume_replay_policy(&dir, retention);
    spawn_supervised_spill_replay(dir, write_url(host, http_port), require_upload, policy)
}

/// The top_volume folder's policy with its age limit set from `retention`.
#[must_use]
pub fn top_volume_replay_policy(
    dir: &Path,
    retention: &tickvault_common::config::PartitionRetentionConfig,
) -> ReplayPolicy {
    ReplayPolicy {
        max_file_age_secs: Some(top_volume_replay_max_age_secs(retention)),
        ..replay_policy_for_dir(dir)
    }
}

fn spawn_supervised_spill_replay(
    dir: PathBuf,
    url: String,
    require_upload: bool,
    policy: ReplayPolicy,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_replay_loop(
                dir.clone(),
                url.clone(),
                require_upload,
                policy,
            ));
            let join_result = handle.await;
            let reason = if join_result.is_ok() {
                "returned"
            } else {
                "panicked"
            };
            error!(
                reason,
                code = tickvault_common::error_code::ErrorCode::TickFlush01WorkerRespawn.code_str(),
                backoff_secs = REPLAY_RESPAWN_BACKOFF_SECS,
                path = %dir.display(),
                "TICK-FLUSH-01: the tick spill drain exited — respawning so spilled ticks keep \
                 being recovered into the database instead of accumulating until they are dropped"
            );
            metrics::counter!("tv_tick_flush_worker_respawn_total", "reason" => reason)
                .increment(1);
            tokio::time::sleep(std::time::Duration::from_secs(REPLAY_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}
#[cfg(test)]
pub(crate) mod tests {
    // Test views shared with `depth_spill_frame`'s tests (audit DA-13b).
    /// Test view: a clean sink with no probes yet and the clock two hours ahead,
    /// so a fixture written just now with a small hour in its name reads as
    /// closed and quiet (unless it is the last file).
    pub(crate) fn test_spill_view() -> SpillSinkView {
        let mut view = SpillSinkView::current();
        view.suspect = false;
        view.clean_probes = 0;
        view.suspect_episodes = 0;
        view.live = false;
        view.episode_start = None;
        view.now_unix_secs = view.now_unix_secs.saturating_add(2 * SPILL_FILE_HOUR_SECS);
        view
    }

    /// Test view: `view` after two clean probes and the durability lag, so the
    /// acks it stamped are confirmed.
    pub(crate) fn confirmed_spill_view(view: &SpillSinkView) -> SpillSinkView {
        let mut later = *view;
        later.clean_probes = later
            .clean_probes
            .saturating_add(SPILL_EMPTY_CONFIRM_PROBES);
        later.now_unix_secs = later
            .now_unix_secs
            .saturating_add(crate::wal_applied_watermark::WATERMARK_DURABILITY_LAG_SECS);
        later
    }

    // ---- 2026-09-03 STREAMING FIX: the tests that bite ----------------------

    /// IST epoch nanos for a seconds-of-day on an arbitrary real trading date.
    fn ist_at(secs_of_day: i64) -> i64 {
        // 2024-05-18 IST-epoch midnight, chosen because 1_716_023_700 (09:15)
        // is the base every other fixture in this workspace counts from.
        1_715_990_400_i64
            .saturating_add(secs_of_day)
            .saturating_mul(1_000_000_000)
    }

    fn tick_line(secs_of_day: i64) -> String {
        format!(
            "ticks,segment=NSE_FNO,feed=dhan ltp=100.5,volume=10i {}\n",
            ist_at(secs_of_day)
        )
    }

    #[test]
    fn an_out_of_window_spill_line_is_refused_and_an_in_window_one_survives() {
        let mut chunk = String::new();
        chunk.push_str(&tick_line(12 * 3600)); // 12:00 — keep
        chunk.push_str(&tick_line(22 * 3600)); // 22:00 — refuse
        chunk.push_str(&tick_line(9 * 3600)); //  09:00 — keep, boundary
        let (kept, dropped) = retain_lines_in_open_window(chunk.as_bytes(), false);

        assert_eq!(dropped, 1, "exactly the 22:00 line is refused");
        let kept = String::from_utf8(kept).expect("utf8");
        assert_eq!(kept.lines().count(), 2);
        assert!(!kept.contains(&ist_at(22 * 3600).to_string()));
        assert!(kept.contains(&ist_at(12 * 3600).to_string()));
        assert!(kept.contains(&ist_at(9 * 3600).to_string()));
    }

    #[test]
    fn a_line_with_no_timestamp_is_kept_because_we_cannot_judge_it() {
        // QuestDB stamps server time for a line with no trailing timestamp.
        // The last token is then a FIELD (`key=value`), never a bare integer.
        // Dropping it would destroy a row on the strength of not understanding
        // it, which is the one error this filter must never make.
        let chunk = b"ticks,segment=NSE_EQ,feed=dhan ltp=100.5,volume=10i\n";
        let (kept, dropped) = retain_lines_in_open_window(chunk, false);
        assert_eq!(dropped, 0);
        assert_eq!(kept, chunk, "kept byte-for-byte");
    }

    #[test]
    fn an_unparseable_or_torn_line_is_kept_never_dropped() {
        // Fail OPEN. Each of these is a shape the parser does not fully
        // understand; keeping them reproduces today's behaviour, while dropping
        // them loses captured market data.
        for raw in [
            &b"not ilp at all\n"[..],
            &b"ticks,segment=NSE_EQ ltp=1.0 notanumber\n"[..],
            &b"ticks,segment=NSE_EQ ltp=1.0 \n"[..],
            &b"\n"[..],
            &b"ticks,segment=NSE_EQ ltp=1.0 17160237"[..], // torn tail, no newline
            &[0xff, 0xfe, b' ', b'1', b'2', b'3', b'\n'][..], // invalid utf8
        ] {
            let (kept, dropped) = retain_lines_in_open_window(raw, false);
            assert_eq!(dropped, 0, "must keep: {raw:?}");
            assert_eq!(kept, raw, "must keep byte-for-byte: {raw:?}");
        }
    }

    #[test]
    fn a_torn_tail_carrying_an_out_of_window_stamp_is_still_refused() {
        // The final read of a crash-torn file has no trailing newline. The
        // stamp is still a stamp, so the judgement still applies.
        let line = format!(
            "ticks,segment=NSE_EQ,feed=dhan ltp=1.0 {}",
            ist_at(23 * 3600)
        );
        let (kept, dropped) = retain_lines_in_open_window(line.as_bytes(), false);
        assert_eq!(dropped, 1);
        assert!(kept.is_empty());
    }

    #[test]
    fn a_torn_timestamp_is_kept_because_its_magnitude_is_not_an_epoch() {
        // BITE: this is the fail-open half of the contract, and it is the half
        // that costs real data when it is wrong. `17160237` is a crash-truncated
        // prefix of a nanosecond stamp. It parses cleanly as an integer and, read
        // as nanoseconds, lands in 1970 -- out of window by any measure. Judging
        // on the window alone would DELETE a captured tick because a crash cut
        // its timestamp short.
        let torn = b"ticks,segment=NSE_EQ ltp=1.0 17160237\n";
        assert!(
            !line_is_positively_out_of_window(torn, false),
            "a torn timestamp must never be judged"
        );

        // The same line with a COMPLETE out-of-window stamp IS judged, so the
        // guard above is not simply disabling the gate.
        let complete = format!("ticks,segment=NSE_EQ ltp=1.0 {}\n", ist_at(23 * 3600));
        assert!(
            line_is_positively_out_of_window(complete.as_bytes(), false),
            "a complete out-of-window stamp must still be refused"
        );
    }

    /// Item 45h: a `feed_aux_packets` row rides the tick spill files and is
    /// stamped at receipt, often outside the session window (a 15:45 tick, a
    /// pre-open connect snapshot). The replay window filter must keep it; the
    /// same stamp on a `ticks` line is still dropped.
    #[test]
    fn a_feed_aux_line_outside_the_window_is_kept_but_a_tick_line_is_not() {
        let late = ist_at(23 * 3600);
        let aux =
            format!("feed_aux_packets,segment=NSE_EQ,kind=out_of_window_tick ltp=1.0 {late}\n");
        assert!(
            !line_is_positively_out_of_window(aux.as_bytes(), false),
            "an aux row must never be judged by the tick window"
        );
        let tick = format!("ticks,segment=NSE_EQ ltp=1.0 {late}\n");
        assert!(line_is_positively_out_of_window(tick.as_bytes(), false));
        let (kept, dropped) = retain_lines_in_open_window(format!("{aux}{tick}").as_bytes(), false);
        assert_eq!(dropped, 1);
        assert_eq!(kept, aux.as_bytes());
        // A table that merely STARTS with the name is not the aux table.
        assert!(!is_feed_aux_line(b"feed_aux_packets_old,x=1 1"));
        assert!(is_feed_aux_line(b"feed_aux_packets x=1 1"));
    }

    #[test]
    fn the_plausible_epoch_band_is_the_aggregator_band_not_a_second_one() {
        // One definition of "this integer is a real market timestamp" in the
        // workspace. If the aggregator's band moves, this moves with it.
        let below = (i64::from(MIN_PLAUSIBLE_EXCHANGE_TS_SECS) - 1) * NANOS_PER_SECOND;
        let at_min = i64::from(MIN_PLAUSIBLE_EXCHANGE_TS_SECS) * NANOS_PER_SECOND;
        let at_max = i64::from(MAX_PLAUSIBLE_EXCHANGE_TS_SECS) * NANOS_PER_SECOND;
        let above = (i64::from(MAX_PLAUSIBLE_EXCHANGE_TS_SECS) + 1) * NANOS_PER_SECOND;

        assert!(!nanos_are_a_plausible_epoch(below));
        assert!(nanos_are_a_plausible_epoch(at_min));
        assert!(nanos_are_a_plausible_epoch(at_max));
        assert!(!nanos_are_a_plausible_epoch(above));

        // The shapes that actually turn up on a torn tail.
        assert!(!nanos_are_a_plausible_epoch(0));
        assert!(!nanos_are_a_plausible_epoch(1));
        assert!(!nanos_are_a_plausible_epoch(-1));
        assert!(!nanos_are_a_plausible_epoch(i64::MIN));
        assert!(!nanos_are_a_plausible_epoch(i64::MAX));
    }

    #[test]
    fn the_filter_preserves_every_byte_of_every_surviving_line() {
        // Concatenation, not reconstruction: the filter must never normalise
        // whitespace, re-order tags, or drop a `\r`. A rewritten line is a
        // different row.
        let odd = format!(
            "ticks,segment=NSE_FNO,feed=dhan  ltp=1.0,x=\"a b\" {}\r\n",
            ist_at(11 * 3600)
        );
        let (kept, dropped) = retain_lines_in_open_window(odd.as_bytes(), false);
        assert_eq!(dropped, 0);
        assert_eq!(kept, odd.as_bytes(), "surviving lines are copied verbatim");
    }

    #[test]
    fn dir_label_is_static_and_names_the_two_real_spill_dirs() {
        assert_eq!(
            dir_label(Path::new(crate::tick_persistence::TICK_SPILL_DIR)),
            "ticks"
        );
        assert_eq!(
            dir_label(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR)),
            "depth"
        );
        assert_eq!(dir_label(Path::new("/tmp/whatever")), "other");
    }

    /// STRUCTURAL BITE-PROOF for the 2026-09-03 OOM.
    ///
    /// MEASURED: a 21 GB depth spill file read whole put process RSS at
    /// 20.96 GiB; `MemoryHigh` throttling then starved the systemd watchdog
    /// ping and the app was SIGABRTed every ~9 minutes. Moving that one file
    /// out of the drain's path dropped RSS to 0.95 GiB. 22x, from one `read`.
    ///
    /// This fails the build if anyone puts a whole-file read back on the
    /// replay path -- including the subtler regression of reading the file
    /// whole and then handing the slice to a still-bounded helper, which
    /// would leave every other test in this module green.
    #[test]
    fn the_replay_path_never_reads_a_whole_spill_file_into_memory() {
        let src = include_str!("tick_spill_replay.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        // Comment-stripped: this guard's OWN docstring names the banned call
        // in order to explain it, and a scanner that cannot tell code from
        // prose fails on its own explanation. The house convention is to
        // strip, not to reword the explanation away.
        let production: String = src
            .split(test_marker)
            .next()
            .unwrap_or(src)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let production = production.as_str();
        for banned in ["fs::read(", "read_to_end", "read_to_string"] {
            assert!(
                !production.contains(banned),
                "`{banned}` on the replay path materialises a WHOLE spill file. There is no \
                 per-file cap: `spill_failed_ilp` appends to one file per feed per clock-hour, \
                 and `SpillCeilingVerdict::OverCeilingWithRoom` lets the directory grow past \
                 its soft ceiling whenever free space is above the database reserve. On \
                 2026-09-03 that reached 21 GB and kill-looped the app."
            );
        }
        assert!(
            production.contains("REPLAY_STREAM_BUFFER_BYTES"),
            "the bounded read buffer must still be what the replay path reads through"
        );
        assert!(
            production.contains("rposition(|b| *b == b'\\n')"),
            "the streaming loop must still cut at the LAST newline in its window. Without that \
             cut a chunk boundary lands mid-line, which hands QuestDB half a row and starts the \
             next body with the other half -- corrupting TWO rows rather than splitting one."
        );
    }

    /// The buffer must hold at least one full POST body, or `ilp_chunk_ranges`
    /// can never emit a full-size chunk and the 8 MiB pacing quietly becomes
    /// something else. Also pins that this change altered how much is READ,
    /// never how much is SENT.
    #[test]
    fn the_read_buffer_is_a_whole_multiple_of_the_post_cap() {
        assert!(REPLAY_STREAM_BUFFER_BYTES >= REPLAY_MAX_CHUNK_BYTES);
        assert_eq!(REPLAY_STREAM_BUFFER_BYTES % REPLAY_MAX_CHUNK_BYTES, 0);
        assert!(
            REPLAY_STREAM_BUFFER_BYTES <= 64 * 1024 * 1024,
            "the buffer is resident for the life of a round -- a large one re-creates the \
             memory pressure this constant exists to bound"
        );
    }

    /// The truncate must not run when the writer appended during the drain.
    ///
    /// `list_spill_files` does not exclude the hour currently being written,
    /// and `File::create` zeroes the shared inode. Pre-existing with the
    /// whole-file read; streaming widens the window from one syscall to
    /// A growing file is left BYTE-FOR-BYTE INTACT and the offset is
    /// remembered.
    ///
    /// Replaces two tests for an in-place compaction DELETED on 2026-09-03
    /// after an adversarial review of the deployed code proved it could erase
    /// a concurrent append. Nothing is moved now, so there is nothing to
    /// corrupt — and this asserts the FILE IS UNCHANGED, which is the property
    /// that makes an appended row impossible to lose.
    #[test]
    fn a_grown_file_is_left_intact_and_the_offset_is_remembered() {
        let dir = std::env::temp_dir().join(format!("tv-resume-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("depth-dhan-000009.ilp");

        let drained = b"old-row-1\nold-row-2\n";
        let appended = b"new-row-1\nnew-row-2\nnew-row-3\n";
        let mut whole = drained.to_vec();
        whole.extend_from_slice(appended);
        std::fs::write(&path, &whole).expect("write fixture");

        super::record_resume_offset(&path, drained.len() as u64);

        let after = std::fs::read(&path).expect("read back");
        assert_eq!(
            after.as_slice(),
            whole.as_slice(),
            "the spill file must be left byte-for-byte intact — every version \
             of this code that MOVED bytes could destroy a concurrent append"
        );
        assert_eq!(super::resume_offset_for(&path), drained.len() as u64);

        super::record_resume_offset(&path, 1);
        assert_eq!(
            super::resume_offset_for(&path),
            drained.len() as u64,
            "the resume offset moved BACKWARDS — that re-sends rows and invites \
             a future edit to treat it as a rewind"
        );

        super::forget_resume_offset(&path);
        assert_eq!(super::resume_offset_for(&path), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grown-file branch must NEITHER truncate NOR move bytes.
    ///
    /// REPLACES a guard that pinned the opposite ("must compact"), written
    /// 2026-09-03 and wrong within hours. It passed on the broken version
    /// because it only checked the branch CALLED the compactor — the same
    /// false-OK shape as the cgroup guard earlier the same day. This one pins
    /// the property that actually makes appends safe: the branch touches
    /// NOTHING on disk. Both destructive answers are banned outright, because
    /// either erases a concurrent append and both read as the tidy finish.
    #[test]
    fn the_grown_file_branch_never_destroys_a_concurrent_append() {
        let src = include_str!("tick_spill_replay.rs");
        let stripped: String = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let branch = stripped
            .find("Ok(len_now) if len_now > file_len")
            .expect("the grown-file branch must exist");
        let tail = &stripped[branch..];
        let end = tail.find("Ok(_) => {}").unwrap_or(tail.len());
        let body = &tail[..end];

        assert!(
            body.contains("record_resume_offset"),
            "the grown-file branch no longer records a resume offset. Without \
             it the drained prefix is re-POSTed every 300s at the database \
             whose slowness caused the spill, and it never converges."
        );
        for destructive in ["set_len", "File::create", "compact_drained_prefix"] {
            assert!(
                !body.contains(destructive),
                "the grown-file branch calls `{destructive}`, which can erase \
                 bytes the writer appended while the drain ran. The file is \
                 held open with `.append(true)`; there is no atomic way to \
                 shorten it, so it must be left BYTE-FOR-BYTE INTACT."
            );
        }
    }

    /// minutes, so the guard lands with the fix.
    #[test]
    fn the_truncate_is_gated_on_the_file_not_having_grown() {
        let src = include_str!("tick_spill_replay.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        let body = &production[production
            .find("pub async fn replay_spill_dir(")
            .expect("replay_spill_dir must exist")..];
        // The empty-out after a drain is the LAST call in the function; the
        // earlier one empties a file drained in an earlier round, which has
        // not grown since (its resume offset still equals its length).
        let create_at = body
            .rfind("empty_drained_closed_file(&path")
            .expect("the truncate site must still exist");
        let guard_at = body
            .find("Ok(len_now) if len_now > file_len")
            .expect("the growth check must exist");
        assert!(
            guard_at < create_at,
            "the growth check sits BELOW the truncate, so a file the writer appended to during \
             the drain is zeroed before the check can refuse it -- destroying rows that were \
             never POSTed"
        );
    }

    use super::*;

    fn cat(payload: &[u8], ranges: &[std::ops::Range<usize>]) -> Vec<u8> {
        let mut out = Vec::new();
        for r in ranges {
            out.extend_from_slice(&payload[r.clone()]);
        }
        out
    }

    #[test]
    fn ilp_chunk_ranges_splits_only_on_newlines() {
        let payload = b"aaaa\nbbbb\ncccc\n";
        let ranges = ilp_chunk_ranges(payload, 6);
        for r in &ranges {
            assert!(
                r.end == payload.len() || payload[r.end - 1] == b'\n',
                "a chunk must end on a newline or at end-of-payload, got {r:?}"
            );
        }
        assert!(
            ranges.len() > 1,
            "the cap must actually have forced a split"
        );
    }

    #[test]
    fn chunks_concatenate_back_to_the_exact_payload() {
        // THE zero-loss property. If the chunker ever drops or duplicates a
        // byte, replay writes the wrong rows — and every other assertion here
        // would still pass.
        for cap in [1usize, 2, 3, 5, 8, 13, 64, 4096] {
            let payload = b"cpu,host=a v=1i 1\ncpu,host=b v=2i 2\ncpu,host=c v=3i 3\n";
            let ranges = ilp_chunk_ranges(payload, cap);
            assert_eq!(
                cat(payload, &ranges),
                payload.to_vec(),
                "cap {cap} lost or duplicated bytes"
            );
        }
    }

    #[test]
    fn ilp_chunk_ranges_keeps_an_over_long_line_whole() {
        // Splitting it corrupts two rows; dropping it is silent loss. Whole is
        // the only option that keeps the data.
        let payload = b"aaaaaaaaaaaaaaaaaaaa\nbb\n";
        let ranges = ilp_chunk_ranges(payload, 4);
        assert_eq!(&payload[ranges[0].clone()], b"aaaaaaaaaaaaaaaaaaaa\n");
        assert_eq!(cat(payload, &ranges), payload.to_vec());
    }

    #[test]
    fn ilp_chunk_ranges_handles_a_payload_with_no_trailing_newline() {
        let payload = b"aaaa\nbbbb";
        let ranges = ilp_chunk_ranges(payload, 5);
        assert_eq!(cat(payload, &ranges), payload.to_vec());
        assert_eq!(ranges.last().expect("a chunk").end, payload.len());
    }

    #[test]
    fn ilp_chunk_ranges_on_empty_or_zero_cap_yields_nothing() {
        assert!(ilp_chunk_ranges(b"", 16).is_empty());
        assert!(
            ilp_chunk_ranges(b"a\n", 0).is_empty(),
            "a zero cap must not loop forever"
        );
    }

    #[test]
    fn list_spill_files_ignores_everything_that_is_not_an_ilp_file() {
        let dir = std::env::temp_dir().join(format!("tv-spill-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("ticks-dhan-1.ilp"), b"x\n").expect("write");
        std::fs::write(dir.join("ticks-dhan-2.ilp"), b"y\n").expect("write");
        std::fs::write(dir.join("notes.txt"), b"z\n").expect("write");
        std::fs::create_dir_all(dir.join("sub.ilp")).expect("subdir");

        let files = list_spill_files(&dir);
        assert_eq!(files.len(), 2, "only the two .ilp FILES may be listed");
        assert!(files[0] < files[1], "oldest hour must drain first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_spill_files_on_a_missing_directory_is_not_an_error() {
        // A box that never spilled has no such directory, and that is health,
        // not a fault to report.
        let missing = std::env::temp_dir().join("tv-spill-does-not-exist-9f3a");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(list_spill_files(&missing).is_empty());
    }

    #[test]
    fn write_url_points_at_the_questdb_write_endpoint() {
        assert_eq!(write_url("localhost", 9000), "http://localhost:9000/write");
    }

    #[test]
    fn register_replay_baseline_pre_registers_both_counters() {
        // No recorder is installed under test, so this asserts the call is
        // total and panic-free; the naming of the counters is pinned by the
        // source scan below.
        register_replay_baseline();
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("pub fn register_replay_baseline")
            .nth(1)
            .expect("register_replay_baseline must exist");
        // The closing brace is written as an escape, not a literal: a bare one
        // inside a string unbalances the brace-depth tracker in
        // .claude/hooks/banned-pattern-scanner.sh, which then stops treating this
        // module as test code and flags every `.expect()` below it.
        let decl = &body[..body.find("\n\u{7d}\n").unwrap_or(body.len())];
        assert!(decl.contains("tv_tick_spill_replayed_bytes_total"));
        assert!(decl.contains("tv_tick_spill_replay_failed_total"));
    }

    #[tokio::test]
    async fn replay_spill_dir_skips_an_empty_file_without_touching_it() {
        // An already-drained file must be left for the age-based pruner. If
        // replay re-truncated it every round its mtime would never age and it
        // would live forever.
        let dir = std::env::temp_dir().join(format!("tv-spill-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&path, b"").expect("write");
        let before = std::fs::metadata(&path).expect("meta").modified().ok();

        let client = crate::http_client::build_probe_client(1).expect("client");
        let outcome = replay_spill_dir(
            &dir,
            "http://127.0.0.1:1/write",
            &client,
            &test_spill_view(),
        )
        .await;

        assert_eq!(outcome.files_skipped_empty, 1);
        assert_eq!(outcome.files_replayed, 0);
        assert_eq!(outcome.files_failed, 0, "an empty file is not a failure");
        let after = std::fs::metadata(&path).expect("meta").modified().ok();
        assert_eq!(before, after, "an empty file must not be rewritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn replay_spill_dir_keeps_the_file_when_questdb_is_unreachable() {
        // The whole point of the tier: an unreachable database must never cost
        // the bytes. Port 1 is reserved and refuses immediately.
        let dir = std::env::temp_dir().join(format!("tv-spill-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&path, b"ticks value=1i 1\n").expect("write");

        let client = crate::http_client::build_probe_client(1).expect("client");
        let outcome = replay_spill_dir(
            &dir,
            "http://127.0.0.1:1/write",
            &client,
            &test_spill_view(),
        )
        .await;

        assert_eq!(outcome.files_failed, 1);
        assert_eq!(outcome.files_replayed, 0);
        assert_eq!(
            std::fs::read(&path).expect("read").len(),
            17,
            "a failed replay must leave the payload byte-for-byte intact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn replay_spill_dir_on_a_missing_directory_is_a_no_op() {
        let missing = std::env::temp_dir().join("tv-spill-replay-absent-4c1e");
        let _ = std::fs::remove_dir_all(&missing);
        let client = crate::http_client::build_probe_client(1).expect("client");
        let outcome = replay_spill_dir(
            &missing,
            "http://127.0.0.1:1/write",
            &client,
            &test_spill_view(),
        )
        .await;
        assert_eq!(outcome, SpillReplayOutcome::default());
    }

    #[test]
    fn describe_send_error_never_echoes_a_url() {
        // reqwest's Display embeds the request URL, which can carry proxy
        // userinfo. Only the kind may reach a log line.
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("pub fn describe_send_error")
            .nth(1)
            .expect("describe_send_error must exist");
        // The closing brace is written as an escape, not a literal: a bare one
        // inside a string unbalances the brace-depth tracker in
        // .claude/hooks/banned-pattern-scanner.sh, which then stops treating this
        // module as test code and flags every `.expect()` below it.
        let decl = &body[..body.find("\n\u{7d}\n").unwrap_or(body.len())];
        assert!(
            !decl.contains("err)") || !decl.contains("format!"),
            "the kind is a &'static str; nothing may be formatted from the error"
        );
        for kind in ["timeout", "connect", "body", "decode", "other"] {
            assert!(decl.contains(kind), "missing kind {kind}");
        }
    }

    #[test]
    fn the_drain_loop_replays_before_it_sleeps() {
        // 2026-08-25. This loop slept `REPLAY_INTERVAL_SECS` BEFORE its first
        // round, and that ordering contributed to losing 1,695,983 ticks on
        // the live box in a single event: boot #1's WAL-replay flush was
        // rescued at 08:31:09, writing 544 MB; the deploy restarted the
        // process at 08:31:56, resetting this timer; boot #2's flush at
        // 08:33:44 was then REFUSED with "tick spill dir at or past its
        // 536870912-byte cap"; and this loop woke at ~08:37:12 and freed that
        // same 544 MB, minutes after the room was needed.
        //
        // Boot is precisely when the largest single flush of the process is
        // attempted, and a deploy restart makes boots happen twice in quick
        // succession while this timer resets through both. Sleeping through
        // that is exactly backwards.
        //
        // Swap the two statements back and this test fails.
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("async fn run_replay_loop")
            .nth(1)
            .expect("the drain loop must exist");
        let loop_body = &body[body.find("loop {").expect("the loop must exist")..];
        let first_sleep = loop_body
            .find("tokio::time::sleep")
            .expect("the loop must still pace itself");
        let first_replay = loop_body
            .find("replay_spill_dir(")
            .expect("the loop must still drain");
        assert!(
            first_replay < first_sleep,
            "the drain must run BEFORE the first sleep — a boot-time backlog is \
             the defining case, not an edge case"
        );
    }

    #[test]
    fn the_drain_round_runs_on_the_blocking_pool() {
        // 2026-10-03 (O(1) sweep S3). The round reads spill files with
        // `std::fs` and the trim lists and deletes files; both must run off
        // the tokio workers the frame drain shares.
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("async fn run_replay_loop")
            .nth(1)
            .expect("the drain loop must exist");
        let body = &body[..body.find("\n}\n").expect("the loop function must end")];
        let blocking = body
            .find("spawn_blocking(")
            .expect("the round must run on the blocking pool");
        for step in ["replay_spill_dir(", "prune_quarantine("] {
            let at = body.find(step).expect("the round step must exist");
            assert!(
                at > blocking,
                "`{step}` must run inside the spawn_blocking closure, not on a worker"
            );
        }
    }

    #[test]
    fn spawn_supervised_tick_spill_replay_uses_a_generous_request_timeout() {
        // The body can be 8 MiB and the server's slowness is WHY the file
        // exists. A probe-length timeout would convert a slow-but-working
        // recovery into a permanent failure loop.
        assert!(
            REPLAY_REQUEST_TIMEOUT_SECS >= 30,
            "a chunk POST needs far longer than the readiness probe's 2s"
        );
        assert!(
            REPLAY_INTERVAL_SECS >= 60,
            "the drain must be unhurried — the rows are safe on disk, and eagerness \
             adds write pressure to a database that just proved it had none to spare"
        );
        // Structural: the supervisor must respawn and must say so with a code.
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("\nfn spawn_supervised_spill_replay(")
            .nth(1)
            .expect("the supervised spawn must exist");
        // The closing brace is written as an escape, not a literal: a bare one
        // inside a string unbalances the brace-depth tracker in
        // .claude/hooks/banned-pattern-scanner.sh, which then stops treating this
        // module as test code and flags every `.expect()` below it.
        let decl = &body[..body.find("\n\u{7d}\n").unwrap_or(body.len())];
        assert!(
            decl.contains("TickFlush01WorkerRespawn"),
            "a silent respawn is the failure mode this supervisor exists to prevent"
        );
        assert!(
            decl.contains("loop {"),
            "the supervisor must respawn, not exit after one death"
        );
    }

    #[test]
    fn test_depth_replay_held_at_covers_08_00_to_the_close_only() {
        // 2026-10-05: the depth spill replay waits from 08:00 to 15:40 IST so
        // it cannot spend the disk burst before the open or compete with live
        // depth during the session.
        assert!(!depth_replay_held_at(8 * 3600 - 1), "07:59:59 drains");
        assert!(depth_replay_held_at(8 * 3600), "08:00 holds");
        assert!(depth_replay_held_at(8 * 3600 + 30 * 60), "08:30 boot holds");
        assert!(depth_replay_held_at(12 * 3600), "midday holds");
        assert!(depth_replay_held_at(56_400 - 1), "15:39:59 holds");
        assert!(!depth_replay_held_at(56_400), "15:40 drains");
        assert!(!depth_replay_held_at(17 * 3600), "17:00 drains");
        assert_eq!(
            DEPTH_REPLAY_HOLD_END_SECS_OF_DAY_IST,
            tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST
        );
    }

    #[test]
    fn test_depth_replay_held_at_is_applied_to_the_depth_directory_only() {
        assert!(is_depth_spill_dir(Path::new(
            crate::depth_persistence::DEPTH_SPILL_DIR
        )));
        assert!(!is_depth_spill_dir(Path::new(
            crate::tick_persistence::TICK_SPILL_DIR
        )));
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("async fn run_replay_loop")
            .nth(1)
            .expect("the replay loop must exist");
        let body = &body[..body.find("\n\u{7d}\n").unwrap_or(body.len())];
        // 2026-10-13 (top_volume spill plan item 5): the hold is the policy's.
        assert_eq!(
            replay_policy_for_dir(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR)).hold,
            ReplayHold::InSession
        );
        assert_eq!(
            replay_policy_for_dir(Path::new(crate::tick_persistence::TICK_SPILL_DIR)).hold,
            ReplayHold::Never,
            "the hold must be scoped to the depth directory, never the tick one"
        );
        assert!(
            body.contains("let held = replay_held(\n            policy.hold,"),
            "a held round must consult the policy and the IST window"
        );
        assert!(
            body.contains("tv_depth_spill_replay_held_total"),
            "every held round must be counted"
        );
        let trim = body.find("prune_quarantine(").expect("trim");
        let held = body.find("let outcome = if held").expect("held branch");
        assert!(
            held < trim,
            "the quarantine trim still runs on a held round"
        );
    }

    use std::time::{Duration, Instant};

    /// How long [`tiny_server`] keeps waiting for the `n`-th connection before
    /// it gives up and lets the thread end.
    ///
    /// Generous on purpose: the tests build their client with
    /// `build_probe_client(5)`, so a legitimate request has a 5-second budget
    /// and a healthy run finishes in milliseconds. This is not a latency knob,
    /// it is the bound that turns "parked forever" into "returned".
    const TINY_SERVER_DEADLINE: Duration = Duration::from_secs(30);

    /// How long one accepted socket may take to deliver its request line.
    ///
    /// Without it a client that connects and then sends nothing parks the read
    /// instead of the accept — the same hang one layer in.
    const TINY_SERVER_READ_TIMEOUT: Duration = Duration::from_secs(5);

    /// A one-shot local HTTP server that answers `n` requests with `status`.
    ///
    /// # Why a real socket rather than a mock
    ///
    /// The success arm of [`replay_spill_dir`] is the arm that matters: it is
    /// what truncates the file and reports the bytes as recovered. A mock would
    /// prove only that the mock was called. Twenty lines of `TcpListener`
    /// exercise the real `reqwest` round trip, the real status check, and the
    /// real truncate — with no QuestDB and no new dependency.
    ///
    /// # Why the accept loop is DEADLINE-BOUNDED (2026-09-06)
    ///
    /// Until 2026-09-06 this performed exactly `n` BLOCKING `accept()` calls
    /// and every caller then joined the thread unconditionally. If the client
    /// opened fewer than `n` TCP connections — pooled-connection reuse, a retry
    /// landing on the same socket, any timing shift — the thread parked forever
    /// in `accept()`, `join()` never returned, and the whole test binary hung.
    ///
    /// That is measured, not theoretical. On CI run 34057591677 the test
    /// `replay_spill_dir_truncates_a_closed_file_and_leaves_the_live_one_intact`
    /// hung for 50 minutes and consumed the entire 60-minute `Coverage & Perf`
    /// job budget, while `Test (storage)` passed on the SAME commit in the same
    /// run in 1m35s. A plain `cargo test` and an llvm-cov-instrumented run
    /// differ only in timing, which is exactly what this shape is sensitive to.
    ///
    /// The deadline does not make a missing connection acceptable — it makes it
    /// LOUD. The thread returns, `join()` returns, and the caller's own
    /// assertions fail on the replay that did not happen, naming the cause in
    /// seconds instead of the job dying at its timeout with no verdict at all.
    ///
    /// The two `n > 1` call sites are the exposed ones (`n = 2` and `n = 3`);
    /// a single unconditional connection cannot under-run its own count.
    ///
    /// # Why the handle yields the SERVED COUNT (2026-09-07)
    ///
    /// An adversarial re-read of this fix found that bounding the loop is not
    /// enough on its own: a caller that only asserts on `replay_spill_dir`'s
    /// outcome cannot tell "the server answered 500" from "the server never
    /// ran at all", because a dead listener is dropped, the client gets
    /// `ECONNREFUSED`, and the replay reports the SAME failure tuple. That
    /// test would have passed green straight through the very hang this
    /// change exists to fix. The thread therefore returns how many requests
    /// it actually served, so a caller can assert the response was DELIVERED
    /// rather than merely that the round failed.
    fn tiny_server(status: &'static str, n: usize) -> (String, std::thread::JoinHandle<usize>) {
        tiny_server_with_deadline(status, n, TINY_SERVER_DEADLINE)
    }

    /// [`tiny_server`] with an explicit deadline, so the bound itself is
    /// testable without a test that waits 30 seconds to prove it.
    fn tiny_server_with_deadline(
        status: &'static str,
        n: usize,
        deadline: Duration,
    ) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // Non-blocking so the loop can NOTICE the deadline. A blocking accept
        // has nothing to notice it with, which is the entire defect.
        listener.set_nonblocking(true).expect("set_nonblocking");
        let handle = std::thread::spawn(move || {
            let give_up_at = Instant::now() + deadline;
            let mut served = 0usize;
            while served < n {
                if Instant::now() >= give_up_at {
                    return served;
                }
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        // The accepted socket must be BLOCKING: it inherits
                        // nothing useful here, and a non-blocking read would
                        // return WouldBlock before the request arrived and we
                        // would answer an empty request.
                        let _ = sock.set_nonblocking(false);
                        let _ = sock.set_read_timeout(Some(TINY_SERVER_READ_TIMEOUT));
                        let mut buf = [0u8; 4096];
                        // One read is enough: we never inspect the body, and
                        // draining it fully would block on a request larger
                        // than the buffer.
                        let _ = sock.read(&mut buf);
                        let _ = sock.write_all(status.as_bytes());
                        let _ = sock.flush();
                        served = served.saturating_add(1);
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        // WouldBlock is the normal no-pending-connection poll.
                        // Interrupted (EINTR) and ConnectionAborted
                        // (ECONNABORTED — a client that vanished between the
                        // SYN and our accept) are TRANSIENT: std does not
                        // retry `accept` for us, so returning here would end
                        // the server on a hiccup and hand the caller the same
                        // never-ran ambiguity the served count exists to
                        // remove. The deadline still bounds the retry.
                        //
                        // 5 ms: short enough that a real connection is served
                        // essentially immediately, long enough that the poll
                        // costs nothing across a whole suite.
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    // Anything else (EMFILE, EBADF, a closed listener) is not
                    // retryable — end, and report what was served so the
                    // caller's assertion names the shortfall.
                    Err(_) => return served,
                }
            }
            served
        });
        (format!("http://127.0.0.1:{port}/write"), handle)
    }

    /// The accept loop must END when the `n`-th connection never arrives.
    ///
    /// This is the regression test for the 2026-09-06 CI hang: it asks for two
    /// connections, makes ZERO, and proves the thread terminated anyway.
    ///
    /// # Why the join runs on a helper thread
    ///
    /// A bite test for a HANG cannot simply call `join()`. If the bound is
    /// removed, that call parks and the TEST hangs rather than failing — which
    /// is the very outcome being fixed, and is worthless as a signal: a hung
    /// test looks identical to a slow one until the job dies at its timeout
    /// with no verdict. Joining on a helper thread and waiting on a channel
    /// converts "parked forever" into a NAMED assertion failure in five
    /// seconds. On failure the helper is deliberately left running: it is
    /// parked in `accept()`, which is exactly what the assertion reports.
    #[test]
    fn tiny_server_gives_up_instead_of_parking_when_a_connection_never_arrives() {
        // Two expected, none made — the shape that hung CI, with the deadline
        // shrunk so the bound is provable without a 30-second test.
        let (_url, server) = tiny_server_with_deadline(
            "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            2,
            Duration::from_millis(200),
        );

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ended_cleanly = server.join().is_ok();
            let _ = tx.send(ended_cleanly);
        });

        let ended_cleanly = rx.recv_timeout(Duration::from_secs(5)).expect(
            "the accept loop must be deadline-bounded, and it never returned. \
             That is the hang which consumed the entire 60-minute Coverage & Perf \
             budget on CI run 34057591677 while Test (storage) passed on the same \
             commit in 1m35s",
        );
        assert!(
            ended_cleanly,
            "the server thread must end by deadline, not by panicking"
        );
    }

    /// The deadline must not cost a HEALTHY run anything: the same helper still
    /// serves every connection it promised.
    ///
    /// Without this, the test above could be satisfied by a server that never
    /// serves anyone — a bound that works by breaking the thing it bounds.
    ///
    /// # Why ONE pooled client, and why that is the point (2026-09-07)
    ///
    /// The first version of this test used two SEPARATE clients so neither
    /// could reuse the other's socket. That proved the deadline was harmless
    /// and proved nothing about the actual failure: every real call site — and
    /// `replay_spill_dir` itself — reuses ONE `build_probe_client` across all
    /// its requests, and `build_probe_client` sets no `pool_max_idle_per_host`,
    /// so reqwest keeps the connection alive by default. Under-connecting was
    /// never a hypothetical; it was the production shape.
    ///
    /// So this now drives two requests through ONE client and asserts the
    /// server SERVED both — the shape that can actually under-run.
    ///
    /// # ⚠ What this does NOT prove, measured 2026-09-07
    ///
    /// The `connection: close` header on every response was added in the same
    /// change on the theory that it is what stops reqwest pooling the socket.
    /// **Bite-tested, and the bite did NOT fire:** removing the header from
    /// this test leaves it passing with `served == 2`. The reason is in the
    /// accept loop above — `sock` is bound inside the `Ok((mut sock, _))` arm
    /// and DROPPED at the end of it, which sends FIN, so the helper closes
    /// every connection server-side whether or not the header says so. The
    /// header is therefore honest hygiene and redundant belt-and-braces, NOT
    /// the load-bearing mechanism, and it is recorded that way rather than
    /// claimed as a fix.
    ///
    /// Which leaves the original under-connection cause UNEXPLAINED. The
    /// deadline is what makes that survivable: whatever the mechanism, the
    /// thread ends and the caller's assertion names the shortfall in seconds
    /// instead of the job dying at its timeout with no verdict.
    #[test]
    fn tiny_server_still_serves_every_connection_it_promised() {
        let (url, server) = tiny_server_with_deadline(
            "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            2,
            Duration::from_secs(30),
        );

        // ONE client, reused — the shape `replay_spill_dir` actually uses.
        let client = crate::http_client::build_probe_client(5).expect("client");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        for _ in 0..2 {
            let sent = rt.block_on(async { client.post(&url).body("x").send().await.is_ok() });
            assert!(sent, "the server must answer a real request");
        }

        let served = server.join().expect("server thread");
        assert_eq!(
            served, 2,
            "a pooled client must still get two connections served — the helper drops each socket after answering, so the count is what proves it"
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-spill-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[tokio::test]
    async fn replay_spill_dir_truncates_a_closed_file_and_leaves_the_live_one_intact() {
        // THE success arm, and the two outcomes are DIFFERENT by design.
        //
        // A CLOSED file (the hour rolled; no writer holds it) is emptied, not
        // deleted, so `prune_spill_files` can still tell a drained file from an
        // abandoned one.
        //
        // The CURRENT file — the last by name, the one the writer is still
        // appending to — is left BYTE-FOR-BYTE INTACT and its drained offset is
        // recorded instead. Emptying it would race the appender between the
        // size check and `File::create`, which is the third variant of the
        // append-destroying bug found on 2026-09-03. Bounded cost: at most one
        // hour of spill stays on disk until the rollover makes it closed.
        let dir = temp_dir("ok");
        let closed = dir.join("ticks-dhan-1.ilp");
        let live = dir.join("ticks-dhan-2.ilp");
        let payload = b"ticks value=1i 1\nticks value=2i 2\n";
        std::fs::write(&closed, payload).expect("write closed");
        std::fs::write(&live, payload).expect("write live");

        let (url, server) = tiny_server(
            "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            2,
        );
        let client = crate::http_client::build_probe_client(5).expect("client");
        let view = test_spill_view();
        let outcome = replay_spill_dir(&dir, &url, &client, &view).await;
        let _ = server.join();

        assert_eq!(outcome.files_replayed, 2);
        assert_eq!(outcome.files_failed, 0);
        assert_eq!(outcome.bytes_replayed, (payload.len() as u64) * 2);
        // Audit DA-13b: the closed file is kept until the acks are confirmed.
        assert_eq!(outcome.files_awaiting_confirm, 1);
        assert_eq!(
            std::fs::metadata(&closed).expect("meta").len(),
            payload.len() as u64,
            "a closed file is emptied only after its acks are confirmed"
        );
        // A later round, two clean probes and the durability lag on: emptied,
        // with nothing POSTed (the server above served its two requests).
        let later = replay_spill_dir(&dir, &url, &client, &confirmed_spill_view(&view)).await;
        assert_eq!(later.files_failed, 0);
        assert_eq!(later.bytes_replayed, 0, "nothing is re-sent");

        assert_eq!(
            std::fs::metadata(&closed).expect("meta").len(),
            0,
            "a drained CLOSED file must be EMPTY, not absent — prune_spill_files \
             distinguishes the two and pages AGGREGATOR-DROP-01 on the other"
        );
        assert_eq!(
            std::fs::read(&live).expect("read live").as_slice(),
            payload.as_slice(),
            "the file the writer is still appending to must be left byte-for-byte \
             intact — truncating it races the appender and destroys whatever \
             landed between the size check and the truncate"
        );
        assert_eq!(
            super::resume_offset_for(&live),
            payload.len() as u64,
            "the live file's drained offset must be recorded, or the next round \
             re-sends the whole file and never converges"
        );
        super::forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_permanently_refused_file_is_quarantined_not_left_in_the_queue() {
        // A 4xx is not a transport failure: the bytes arrived and were
        // rejected, so no number of retries can change the outcome. The file
        // must SURVIVE byte-for-byte — the operator's manual replay of the
        // salvageable lines is still the recovery — but it must leave the
        // drain queue, because leaving it there strands every file behind it.
        //
        // Before 2026-08-25 this test asserted the file stayed put. That was
        // the bug: on that day one torn line kept a 512 MB file holding
        // 1,662,318 intact ticks from ever being attempted.
        let dir = temp_dir("refused");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&path, b"ticks value=1i 1\n").expect("write");

        let (url, server) = tiny_server(
            "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            1,
        );
        let client = crate::http_client::build_probe_client(5).expect("client");
        let outcome = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        let _ = server.join();

        assert_eq!(outcome.files_quarantined, 1);
        assert_eq!(outcome.files_replayed, 0);
        assert_eq!(outcome.bytes_replayed, 0);
        assert_eq!(
            outcome.files_failed, 0,
            "a quarantined file is handled, not failed — counting it as a \
             failure would keep reporting a backlog that is no longer blocked"
        );
        assert!(!path.exists(), "it must leave the drain queue");
        let moved = dir.join(QUARANTINE_DIR).join("ticks-dhan-1.ilp");
        assert_eq!(
            std::fs::read(&moved).expect("read"),
            b"ticks value=1i 1\n",
            "a refused replay must leave the payload byte-for-byte intact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_file_stops_the_whole_round() {
        // The spill exists because the database was already too slow. Pushing
        // the rest of the backlog at it is how a bounded tick loss becomes an
        // unbounded outage — so the SECOND file must not even be attempted.
        let dir = temp_dir("stop");
        let first = dir.join("ticks-dhan-1.ilp");
        let second = dir.join("ticks-dhan-2.ilp");
        std::fs::write(&first, b"ticks value=1i 1\n").expect("write");
        std::fs::write(&second, b"ticks value=2i 2\n").expect("write");

        let (url, server) = tiny_server(
            "HTTP/1.1 500 Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            1,
        );
        let client = crate::http_client::build_probe_client(5).expect("client");
        let outcome = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        let served = server.join().expect("server thread");

        // The response must have been DELIVERED, not merely absent.
        //
        // Without this line the test is VACUOUS: a server that never ran drops
        // its listener, the client gets ECONNREFUSED, and `replay_spill_dir`
        // reports the identical tuple below — one file failed, none replayed,
        // second file untouched. That is exactly the state the 2026-09-06 CI
        // hang produced, so this assertion is what makes the rest of the test
        // mean "the round stops on a 500" instead of "the round stops".
        assert_eq!(served, 1, "the 500 must actually have been served");
        assert_eq!(outcome.files_failed, 1, "exactly ONE file may be attempted");
        assert_eq!(outcome.files_replayed, 0);
        assert!(
            !std::fs::read(&second).expect("read").is_empty(),
            "the second file must be untouched — the round stops at the first failure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn replay_spill_dir_drains_every_file_when_all_succeed() {
        // The multi-file loop: oldest hour first, and the round continues.
        let dir = temp_dir("many");
        for n in 1..=3 {
            std::fs::write(dir.join(format!("ticks-dhan-{n}.ilp")), b"ticks v=1i 1\n")
                .expect("write");
        }
        let (url, server) = tiny_server(
            "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            3,
        );
        let client = crate::http_client::build_probe_client(5).expect("client");
        let outcome = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        let _ = server.join();

        assert_eq!(outcome.files_replayed, 3);
        assert_eq!(outcome.files_failed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn describe_send_error_reports_connect_and_timeout_by_kind() {
        // Both real failures, because the point of this function is that a
        // reqwest error's Display embeds the URL and must never be logged.
        let client = crate::http_client::build_probe_client(1).expect("client");

        let refused = client
            .post("http://127.0.0.1:1/write")
            .body(vec![1u8])
            .send()
            .await
            .expect_err("port 1 must refuse");
        assert_eq!(describe_send_error(&refused), "connect");

        // A listener that accepts and never answers: the client times out.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hang = std::thread::spawn(move || {
            let _keep = listener.accept();
            std::thread::sleep(std::time::Duration::from_millis(2_500));
        });
        let timed_out = client
            .post(format!("http://127.0.0.1:{port}/write"))
            .body(vec![1u8])
            .send()
            .await
            .expect_err("a server that never answers must time out");
        assert_eq!(describe_send_error(&timed_out), "timeout");
        let _ = hang.join();
    }

    #[tokio::test]
    async fn the_supervised_drain_starts_and_stays_running() {
        // `run_replay_loop` builds its HTTP client and pre-registers both
        // counters BEFORE its first sleep, so a plain spawn-and-yield covers
        // that entry path — and the assertion that matters is that the
        // supervisor is still alive, because a drain that exits quietly is how
        // rescued ticks accumulate to the 512 MiB cap unnoticed.
        //
        // Paused time is deliberately NOT used: it needs tokio's `test-util`
        // feature, and adding a dependency feature to reach one sleep would be
        // a worse trade than covering the entry path directly.
        let dir = temp_dir("supervised");
        let handle = spawn_supervised_tick_spill_replay(dir.clone(), "127.0.0.1", 1, true);
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "the supervisor must keep running — a drain that exits quietly is \
             how rescued ticks accumulate to the cap unnoticed"
        );
        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The third drain (top_volume spill plan item 5) is supervised the same
    /// way. Its folder is the production relative path, which does not exist
    /// under the test's working directory, so every round finds nothing.
    #[tokio::test]
    async fn test_spawn_supervised_top_volume_spill_replay_starts_and_stays_running() {
        let retention = tickvault_common::config::PartitionRetentionConfig::default();
        let handle = spawn_supervised_top_volume_spill_replay("127.0.0.1", 1, true, &retention);
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "the supervisor must keep running");
        handle.abort();
    }

    /// Names the three policy builders the replay uses, end to end.
    #[test]
    fn test_replay_policy_for_dir_top_volume_replay_policy_and_top_volume_replay_max_age_secs_agree()
     {
        let dir = Path::new(crate::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR);
        let retention = tickvault_common::config::PartitionRetentionConfig::default();
        let base = replay_policy_for_dir(dir);
        let full = top_volume_replay_policy(dir, &retention);
        assert_eq!(base.max_file_age_secs, None);
        assert_eq!(
            full.max_file_age_secs,
            Some(top_volume_replay_max_age_secs(&retention))
        );
        assert_eq!(
            ReplayPolicy {
                max_file_age_secs: None,
                ..full
            },
            base
        );
        assert!(top_volume_replay_max_age_secs(&retention) >= 86_400);
    }

    /// The permanent-vs-transient split IS the fix. A 5xx must still stop the
    /// round (pushing a backlog at a struggling server is how a bounded loss
    /// becomes an outage); a 4xx must not, because retrying a malformed byte
    /// can never succeed.
    #[test]
    fn permanent_refusal_is_4xx_except_the_two_that_mean_try_again() {
        for s in [400, 401, 403, 404, 413, 422] {
            assert!(
                is_permanent_refusal(s),
                "{s} is a payload-level refusal — retrying it forever blocks the queue"
            );
        }
        // Transient by meaning despite being 4xx by number.
        assert!(!is_permanent_refusal(408), "408 is a timeout — it retries");
        assert!(
            !is_permanent_refusal(429),
            "429 is a rate limit — it retries"
        );
        // Server-side: the original stop-the-round reasoning still applies.
        for s in [500, 502, 503, 504] {
            assert!(
                !is_permanent_refusal(s),
                "{s} means QuestDB is struggling — the round must still stop"
            );
        }
        assert!(!is_permanent_refusal(200), "success is not a refusal");
    }

    /// Quarantine MOVES, never deletes. On 2026-08-25 the refused file still
    /// held 1,292 recoverable lines out of 1,293; a rescue tier that destroys
    /// what it cannot parse is worse than the loss it exists to prevent.
    #[test]
    fn quarantine_moves_the_file_and_preserves_every_byte() {
        let dir = temp_dir("quarantine");
        let spill = dir.join("ticks-dhan-1.ilp");
        let body = b"ticks,segment=NSE_EQ,feed=dhan security_id=1i 1\ntorn";
        std::fs::write(&spill, body).expect("write"); // APPROVED: test

        let moved = quarantine_spill_file(&dir, &spill).expect("quarantine"); // APPROVED: test

        assert!(!spill.exists(), "the file must leave the drain queue");
        assert!(moved.exists(), "and must still exist in quarantine");
        assert_eq!(
            std::fs::read(&moved).expect("read back"), // APPROVED: test
            body,
            "quarantine must preserve every byte — the surviving lines are \
             what the operator salvages"
        );
        assert_eq!(
            moved.parent().and_then(std::path::Path::file_name),
            Some(std::ffi::OsStr::new(QUARANTINE_DIR)),
            "it must land in the quarantine directory, not somewhere ad hoc"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-10-02: spill names recur per feed and hour, and a plain rename
    /// onto an already-quarantined file of the same name destroyed it.
    #[test]
    fn test_regression_quarantine_never_overwrites_an_existing_file() {
        let dir = temp_dir("quarantine-no-overwrite");
        let spill = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&spill, b"first").expect("write"); // APPROVED: test
        let first = quarantine_spill_file(&dir, &spill).expect("q1"); // APPROVED: test
        std::fs::write(&spill, b"second").expect("write"); // APPROVED: test
        let second = quarantine_spill_file(&dir, &spill).expect("q2"); // APPROVED: test
        std::fs::write(&spill, b"third").expect("write"); // APPROVED: test
        let third = quarantine_spill_file(&dir, &spill).expect("q3"); // APPROVED: test
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_eq!(std::fs::read(&first).expect("r"), b"first"); // APPROVED: test
        assert_eq!(std::fs::read(&second).expect("r"), b"second"); // APPROVED: test
        assert_eq!(std::fs::read(&third).expect("r"), b"third"); // APPROVED: test
        assert!(second.to_string_lossy().ends_with("ticks-dhan-1.ilp.1"));
        assert!(third.to_string_lossy().ends_with("ticks-dhan-1.ilp.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_free_quarantine_path_counts_a_dangling_link_as_occupied() {
        let dir = temp_dir("quarantine-free");
        let name = std::ffi::OsStr::new("x.ilp");
        assert_eq!(
            free_quarantine_path(&dir, name).expect("free"), // APPROVED: test
            dir.join("x.ilp")
        );
        std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("x.ilp")).expect("link"); // APPROVED: test
        assert_eq!(
            free_quarantine_path(&dir, name).expect("free"), // APPROVED: test
            dir.join("x.ilp.1")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_depth_spill_dir_names_only_the_depth_directory() {
        assert!(is_depth_spill_dir(Path::new(
            crate::depth_persistence::DEPTH_SPILL_DIR
        )));
        assert!(!is_depth_spill_dir(Path::new(
            crate::tick_persistence::TICK_SPILL_DIR
        )));
        assert!(!is_depth_spill_dir(Path::new("/tmp/whatever")));
    }

    /// The depth quarantine is trimmed against the depth ceiling, not the
    /// tick one (until 2026-10-02 both used the tick ceiling).
    #[test]
    fn test_spill_max_bytes_for_dir_gives_depth_its_own_budget() {
        assert_eq!(
            spill_max_bytes_for_dir(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR)),
            crate::depth_persistence::depth_spill_max_bytes()
        );
        assert_eq!(
            spill_max_bytes_for_dir(Path::new(crate::tick_persistence::TICK_SPILL_DIR)),
            crate::tick_persistence::tick_spill_max_bytes()
        );
        assert_eq!(
            spill_max_bytes_for_dir(Path::new("/tmp/whatever")),
            crate::tick_persistence::tick_spill_max_bytes()
        );
    }

    /// A one-shot HTTP responder: 400 for a payload containing `poison`,
    /// 204 otherwise. Hand-rolled on tokio rather than pulling a mock-server
    /// dependency in, because adding a crate needs operator approval and this
    /// is thirty lines.
    async fn spawn_selective_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind"); // APPROVED: test
        let addr = listener.local_addr().expect("addr"); // APPROVED: test
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0_u8; 65536];
                let n = sock.read(&mut buf).await.unwrap_or(0); // APPROVED: test
                let body = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if body.contains("poison") {
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/write"), handle)
    }

    /// The behaviour that actually stranded the data: a permanently-refused
    /// file must not prevent the NEXT file from being attempted.
    ///
    /// Bite-proof: make `is_permanent_refusal` return `false` and this fails
    /// at `files_replayed`, because the round stops at the poison file and the
    /// intact one behind it is never posted — precisely the live shape on
    /// 2026-08-25.
    #[tokio::test]
    async fn a_poison_file_does_not_strand_the_files_behind_it() {
        let dir = temp_dir("poison");
        // Oldest first by name, so `-1` is attempted before `-2`.
        std::fs::write(dir.join("ticks-dhan-1.ilp"), b"poison\n").expect("w1"); // APPROVED: test
        std::fs::write(
            dir.join("ticks-dhan-2.ilp"),
            b"ticks,segment=NSE_EQ,feed=dhan security_id=1i 1\n",
        )
        .expect("w2"); // APPROVED: test

        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();

        assert_eq!(
            out.files_quarantined, 1,
            "the poison file must be set aside"
        );
        assert_eq!(
            out.files_replayed, 1,
            "and the file BEHIND it must still be replayed — this is the whole \
             defect: on 2026-08-25 a 512 MB file holding 1,662,318 intact ticks \
             was never once attempted, because a 401 KB file ahead of it kept \
             being refused and the round stopped there every time"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- PR17 (2026-10-04): one torn line no longer costs the whole file ---

    #[test]
    fn split_at_line_boundary_splits_whole_lines_near_the_middle() {
        let bytes = b"aa\nbb\ncc\ndd\n";
        let split = split_at_line_boundary(bytes, 0..bytes.len()).expect("splits");
        assert_eq!(split, 6, "the boundary nearest the middle");
        assert_eq!(&bytes[..split], b"aa\nbb\n");
        // A single line, with or without its newline, does not split.
        assert_eq!(split_at_line_boundary(b"aa\n", 0..3), None);
        assert_eq!(split_at_line_boundary(b"aaaa", 0..4), None);
        assert_eq!(split_at_line_boundary(b"", 0..0), None);
        // A sub-range of two lines splits between them.
        assert_eq!(split_at_line_boundary(bytes, 3..9), Some(6));
        // A torn tail (no final newline) is its own line.
        let torn = b"aa\nbbbbbbbb";
        assert_eq!(split_at_line_boundary(torn, 0..torn.len()), Some(3));
    }

    /// The defect: a torn line in the middle of a spill file used to
    /// quarantine the whole file, so the intact rows behind it never reached
    /// QuestDB. Now only the refused line is set aside, kept byte for byte.
    ///
    /// Bite-proof: make `isolate_refused_lines` return `WholeFile` and this
    /// fails at `files_quarantined`.
    #[tokio::test]
    async fn a_torn_line_is_set_aside_and_the_rest_of_the_file_is_replayed() {
        let dir = temp_dir("torn-mid");
        let path = dir.join("ticks-dhan-1.ilp");
        let body = b"ticks,segment=NSE_EQ,feed=dhan security_id=1i 1\n\
ticks,segment=NSE_EQ,feed=dhan security_id=2i 2\n\
ticks,segment=NSE_EQ,feed=dhan secupoison\n\
ticks,segment=NSE_EQ,feed=dhan security_id=3i 3\n\
ticks,segment=NSE_EQ,feed=dhan security_id=4i 4\n";
        std::fs::write(&path, body).expect("write"); // APPROVED: test

        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();

        assert_eq!(
            out.files_quarantined, 0,
            "one bad line must not quarantine the file"
        );
        assert_eq!(out.files_failed, 0);
        assert_eq!(out.files_replayed, 1);
        assert_eq!(out.lines_set_aside, 1);
        let kept = std::fs::read(
            dir.join(QUARANTINE_DIR)
                .join(format!("ticks-dhan-1.ilp{REJECTED_LINES_SUFFIX}")),
        )
        .expect("the refused line is kept"); // APPROVED: test
        assert_eq!(kept, b"ticks,segment=NSE_EQ,feed=dhan secupoison\n");
        let line = b"ticks,segment=NSE_EQ,feed=dhan secupoison\n".len() as u64;
        assert_eq!(
            out.bytes_replayed,
            body.len() as u64 - line,
            "every other byte reached QuestDB"
        );
        forget_resume_offset(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A torn TAIL (the crash cut the last append) is set aside the same way
    /// and gets a newline in the kept file, so the next set-aside cannot join
    /// it.
    #[tokio::test]
    async fn a_torn_tail_is_set_aside_with_a_newline() {
        let dir = temp_dir("torn-tail");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(
            &path,
            b"ticks,segment=NSE_EQ,feed=dhan security_id=1i 1\nticks,poison-torn",
        )
        .expect("write"); // APPROVED: test
        // A later hour's file, so `-1` is closed: only a CLOSED file's last
        // line without a newline is a crash-torn tail (audit DA-13b). On the
        // live file it is a line still being written and is left alone.
        std::fs::write(
            dir.join("ticks-dhan-2.ilp"),
            b"ticks,segment=NSE_EQ,feed=dhan security_id=2i 2\n",
        )
        .expect("write"); // APPROVED: test

        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();

        assert_eq!(out.files_quarantined, 0);
        assert_eq!(out.lines_set_aside, 1);
        let kept = std::fs::read(
            dir.join(QUARANTINE_DIR)
                .join(format!("ticks-dhan-1.ilp{REJECTED_LINES_SUFFIX}")),
        )
        .expect("kept"); // APPROVED: test
        assert_eq!(kept, b"ticks,poison-torn\n");
        forget_resume_offset(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An HTTP responder that reads the WHOLE body (by `Content-Length`, so a
    /// full 8 MiB chunk is answered only after it arrived): 400 for a body
    /// containing `poison`, 503 for `busy` when `busy_is_transient`, 204
    /// otherwise.
    async fn spawn_full_body_server(
        busy_is_transient: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind"); // APPROVED: test
        let addr = listener.local_addr().expect("addr"); // APPROVED: test
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req: Vec<u8> = Vec::new();
                let mut buf = vec![0_u8; 65536];
                let mut body_start = None;
                let mut content_len = 0_usize;
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0); // APPROVED: test
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if body_start.is_none()
                        && let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        body_start = Some(i + 4);
                        let head = String::from_utf8_lossy(&req[..i]).to_ascii_lowercase();
                        content_len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                    if let Some(start) = body_start
                        && req.len() >= start + content_len
                    {
                        break;
                    }
                }
                let body = &req[body_start.unwrap_or(req.len()).min(req.len())..];
                let has = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
                let resp = if has(b"poison") {
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if busy_is_transient && has(b"busy") {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/write"), handle)
    }

    /// Audit N1 (2026-10-04): a transient failure LATER in a file used to
    /// throw away what the round had finished, so the next round restarted
    /// at the old offset and set the same refused line aside a second time.
    ///
    /// The file holds a full first chunk with one torn line and a second
    /// chunk QuestDB answers 503. Round 1 sets the torn line aside, then
    /// stops at the 503. Round 2 must resume at the second chunk.
    ///
    /// Bite-proof: drop the `record_resume_offset` in the `failed` branch and
    /// round 2 re-sends the first chunk, isolates the torn line again, and the
    /// kept file holds it twice.
    #[tokio::test]
    async fn a_failure_later_in_a_file_does_not_set_the_same_line_aside_twice() {
        let dir = temp_dir("n1-resume");
        let path = dir.join("ticks-dhan-1.ilp");
        let torn: &[u8] = b"ticks,segment=NSE_EQ,feed=dhan secupoison\n";
        let mut body: Vec<u8> = Vec::with_capacity(REPLAY_MAX_CHUNK_BYTES + 4096);
        body.extend_from_slice(torn);
        let mut sid: u64 = 0;
        while body.len() <= REPLAY_MAX_CHUNK_BYTES {
            sid += 1;
            body.extend_from_slice(
                format!("ticks,segment=NSE_EQ,feed=dhan security_id={sid}i 1\n").as_bytes(),
            );
        }
        body.extend_from_slice(b"ticks,segment=NSE_EQ,feed=dhan busy=1i 1\n");
        std::fs::write(&path, &body).expect("write"); // APPROVED: test
        let client = crate::http_client::build_probe_client(30).expect("client"); // APPROVED: test

        let (url, server) = spawn_full_body_server(true).await;
        let first = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();
        assert_eq!(first.lines_set_aside, 1, "round 1 sets the torn line aside");
        assert_eq!(first.files_failed, 1, "and stops at the 503");
        assert!(
            resume_offset_for(&path) > 0,
            "the finished first chunk is remembered"
        );

        let (url, server) = spawn_full_body_server(false).await;
        let second = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();
        assert_eq!(second.files_failed, 0);
        assert_eq!(
            second.lines_set_aside, 0,
            "round 2 resumes past the first chunk and never isolates the torn line again"
        );
        let kept = std::fs::read(
            dir.join(QUARANTINE_DIR)
                .join(format!("ticks-dhan-1.ilp{REJECTED_LINES_SUFFIX}")),
        )
        .expect("kept"); // APPROVED: test
        assert_eq!(kept, torn, "the torn line is kept exactly once");
        forget_resume_offset(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spill names recur (one per feed and hour). A quarantined file must not
    /// leave its resume offset for the next file of the same name, or that
    /// file starts part-way in and its first bytes are never replayed.
    ///
    /// Bite-proof: drop the `forget_resume_offset` in `quarantine_spill_file`
    /// and the offset survives the move.
    #[test]
    fn quarantining_a_file_forgets_its_resume_offset() {
        let dir = temp_dir("quarantine-forgets-offset");
        let spill = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&spill, b"aaaa\nbbbb\n").expect("write"); // APPROVED: test
        record_resume_offset(&spill, 5);
        assert_eq!(resume_offset_for(&spill), 5);
        quarantine_spill_file(&dir, &spill).expect("quarantine"); // APPROVED: test
        assert_eq!(
            resume_offset_for(&spill),
            0,
            "a new file of the same name starts at the beginning"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refusal no single line explains (every line refused: the table, not
    /// a row) still quarantines the whole file, as before PR17.
    #[tokio::test]
    async fn a_chunk_with_every_line_refused_still_quarantines_the_file() {
        let dir = temp_dir("all-refused");
        let path = dir.join("ticks-dhan-1.ilp");
        let body = b"poison 1\npoison 2\npoison 3\n";
        std::fs::write(&path, body).expect("write"); // APPROVED: test

        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();

        assert_eq!(out.files_quarantined, 1);
        assert_eq!(out.lines_set_aside, 0);
        assert_eq!(
            std::fs::read(dir.join(QUARANTINE_DIR).join("ticks-dhan-1.ilp")).expect("moved"), // APPROVED: test
            body.to_vec(),
            "the file is moved intact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The depth spill is ARRIVAL-clocked, so replay must judge it on the same
    /// clock the depth WRITER used — otherwise replay deletes precisely the rows
    /// the writer was just taught to keep.
    ///
    /// 15:41 IST is past the 15:39:59 window end but inside
    /// `ARRIVAL_GRACE_TAIL_SECS`. `DepthWriter` accepts such a book (the vendor's
    /// measured lag is p99 46.4 s, max 198.7 s). Before this fix, replay judged it
    /// on the exchange clock and DELETED it — and because a fully-refused file is
    /// truncated like any other, those bytes were gone permanently.
    ///
    /// Bite-proof in both directions: the first assert fails if the depth arm is
    /// reverted to the strict clock; the second fails if the grace is wrongly
    /// applied to exchange-clocked spills, which would admit genuinely late ticks.
    #[test]
    fn a_late_depth_spill_line_survives_replay_but_the_same_tick_line_does_not() {
        let late = 15 * 3600 + 41 * 60; // 15:41 IST — inside the arrival grace
        let chunk = tick_line(late);

        let (kept_depth, dropped_depth) =
            retain_lines_in_open_window(chunk.as_bytes(), /* is_arrival_clock */ true);
        assert_eq!(
            dropped_depth, 0,
            "a 15:41 ARRIVAL-clocked row is inside the grace the depth writer \
             accepts; replay must not delete it (the file is truncated either \
             way, so a refusal here is permanent loss)"
        );
        assert_eq!(
            String::from_utf8(kept_depth).expect("utf8").lines().count(),
            1
        );

        let (kept_tick, dropped_tick) =
            retain_lines_in_open_window(chunk.as_bytes(), /* is_arrival_clock */ false);
        assert_eq!(
            dropped_tick, 1,
            "the SAME stamp on an EXCHANGE-clocked spill is genuinely out of \
             window — the grace must not leak onto the tick path"
        );
        assert!(kept_tick.is_empty());
    }

    /// The clock is chosen from the directory being drained, so the depth dir
    /// must be the one that maps to the arrival clock. Pins the mapping itself,
    /// not just the predicate: a rename of `DEPTH_SPILL_DIR` that missed the
    /// replay call site would otherwise silently restore the loss.
    #[test]
    fn the_depth_spill_dir_is_the_one_that_selects_the_arrival_clock() {
        use std::path::Path;
        let depth = dir_label(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR));
        let tick = dir_label(Path::new(crate::tick_persistence::TICK_SPILL_DIR));
        assert_ne!(
            depth, tick,
            "the two spill dirs must be distinguishable, or replay cannot pick a clock"
        );
        assert_eq!(
            depth, "depth",
            "the replay call site compares against this label"
        );
    }

    // --- Audit DA-13b (2026-10-13): when a drained spill file may be emptied ---

    const GOOD_LINE: &[u8] = b"ticks,segment=NSE_EQ,feed=dhan security_id=1i 1\n";

    /// C4 / C15 / split 0: a 2xx is not proof of durable application. A
    /// drained closed file is emptied only once two clean probes ran after
    /// its last ack, the durability lag passed, and the sink is not suspect.
    ///
    /// Bite: make `drained_file_empty_verdict` return `Ok(())` and the file
    /// is emptied in the first round.
    #[tokio::test]
    async fn test_regression_da13b_a_drained_closed_file_waits_for_confirmed_acks() {
        let dir = temp_dir("da13b-confirm");
        let closed = dir.join("ticks-dhan-1.ilp");
        let live = dir.join("ticks-dhan-2.ilp");
        std::fs::write(&closed, GOOD_LINE).expect("write"); // APPROVED: test
        std::fs::write(&live, GOOD_LINE).expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let len = GOOD_LINE.len() as u64;
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);

        let view = test_spill_view();
        let first = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(first.bytes_replayed, 2 * len);
        assert_eq!(first.files_awaiting_confirm, 1);
        assert_eq!(
            size(&closed),
            len,
            "not emptied on the round that POSTed it"
        );

        // One clean probe only.
        let mut one_probe = confirmed_spill_view(&view);
        one_probe.clean_probes = view.clean_probes + 1;
        let _ = replay_spill_dir(&dir, &url, &client, &one_probe).await;
        assert_eq!(size(&closed), len, "one clean probe can race the ack");

        // Two probes, inside the durability lag.
        let mut early = confirmed_spill_view(&view);
        early.now_unix_secs = view.now_unix_secs + 1;
        let _ = replay_spill_dir(&dir, &url, &client, &early).await;
        assert_eq!(size(&closed), len, "inside the durability lag");

        // Everything else confirmed, but the sink is suspect now.
        let mut suspect = confirmed_spill_view(&view);
        suspect.suspect = true;
        let _ = replay_spill_dir(&dir, &url, &client, &suspect).await;
        assert_eq!(size(&closed), len, "a suspect sink empties nothing");

        let done = replay_spill_dir(&dir, &url, &client, &confirmed_spill_view(&view)).await;
        server.abort();
        assert_eq!(done.bytes_replayed, 0, "nothing re-sent");
        assert_eq!(size(&closed), 0, "emptied once the acks are confirmed");
        assert_eq!(size(&live), len, "the live file is never emptied");
        forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C4 / split 0: a suspicion episode that began after the ack (even one
    /// that ended between two rounds) means the ack may have been a lie, so
    /// the file is POSTed again rather than emptied.
    ///
    /// Bite: make `rewind_after_suspicion` never rewind and round 2 empties
    /// the file with nothing re-sent.
    #[tokio::test]
    async fn test_regression_da13b_a_suspicion_episode_after_the_ack_rewinds_the_file() {
        let dir = temp_dir("da13b-rewind");
        let closed = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&closed, GOOD_LINE).expect("write"); // APPROVED: test
        std::fs::write(dir.join("ticks-dhan-2.ilp"), b"").expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let len = GOOD_LINE.len() as u64;

        let view = test_spill_view();
        let first = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(first.bytes_replayed, len);

        // Probes and lag satisfied, but an episode began (and ended) since.
        let mut after_episode = confirmed_spill_view(&view);
        after_episode.suspect_episodes = view.suspect_episodes + 1;
        let second = replay_spill_dir(&dir, &url, &client, &after_episode).await;
        assert_eq!(second.bytes_replayed, len, "the file is POSTed again");
        assert_eq!(
            std::fs::metadata(&closed).expect("m").len(),
            len,
            "and kept until the new acks are confirmed"
        );

        let third =
            replay_spill_dir(&dir, &url, &client, &confirmed_spill_view(&after_episode)).await;
        server.abort();
        assert_eq!(third.bytes_replayed, 0);
        assert_eq!(std::fs::metadata(&closed).expect("m").len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding (MED): an ack confirmed BEFORE a suspicion episode that
    /// began and ended between two rounds is settled by the episode's
    /// recorded start, not rewound; an ack the episode could have touched
    /// still is.
    ///
    /// Bite: drop the `settled_before_episode` arm of `confirmed_at` and the
    /// first rewind returns true.
    #[test]
    fn test_regression_da13b_an_ack_confirmed_before_an_episode_began_is_settled() {
        let path = temp_dir("da13b-episode-start").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        note_spill_ack(&path, &view, 0, 42);
        record_resume_offset(&path, 42);
        let lag = crate::wal_applied_watermark::WATERMARK_DURABILITY_LAG_SECS;
        let mut after_episode = view;
        after_episode.suspect_episodes = view.suspect_episodes + 1;
        after_episode.clean_probes = view.clean_probes + 5;
        after_episode.now_unix_secs = view.now_unix_secs + 300;
        // The episode began after two clean probes and the lag had passed.
        after_episode.episode_start = Some(EpisodeStart {
            episode: after_episode.suspect_episodes,
            clean_probes: view.clean_probes + SPILL_EMPTY_CONFIRM_PROBES,
            unix_secs: view.now_unix_secs + lag,
        });
        assert!(
            !rewind_after_suspicion(&path, &after_episode, "ticks"),
            "confirmed before the episode began: settled, never re-sent"
        );
        assert_eq!(resume_offset_for(&path), 42);
        assert_eq!(drained_file_empty_verdict(&path, &after_episode), Ok(()));

        // The same, but the episode began after only one clean probe: the
        // ack may be a lie, so its bytes are sent again.
        note_spill_ack(&path, &view, 42, 84);
        record_resume_offset(&path, 84);
        let mut early = after_episode;
        early.episode_start = Some(EpisodeStart {
            episode: early.suspect_episodes,
            clean_probes: view.clean_probes + 1,
            unix_secs: view.now_unix_secs + lag,
        });
        // Two episodes since the ack, or no start record: doubt re-sends.
        let mut two_episodes = after_episode;
        two_episodes.suspect_episodes += 1;
        assert!(drained_file_empty_verdict(&path, &two_episodes).is_err());
        assert!(rewind_after_suspicion(&path, &early, "ticks"));
        assert_eq!(
            resume_offset_for(&path),
            42,
            "rewound to where the unconfirmed acks began, not to 0"
        );
        forget_resume_offset(&path);
    }

    /// A live view's ack is stamped with the clock read AT the ack, not the
    /// round's start: probes and time that passed during a long round, before
    /// the ack, must not count towards its confirmation.
    ///
    /// Bite: stamp `view.now_unix_secs` for a live view and the verdict
    /// below reads `Ok`.
    #[test]
    fn test_regression_da13b_a_live_ack_is_stamped_at_the_ack_not_the_round_start() {
        let path = temp_dir("da13b-live-stamp").join("ticks-dhan-1.ilp");
        let mut round_start = test_spill_view();
        round_start.live = true;
        round_start.now_unix_secs = 0;
        note_spill_ack(&path, &round_start, 0, 1);
        let mut later = round_start;
        later.live = false;
        later.clean_probes = u64::MAX / 2;
        later.now_unix_secs = 1_000;
        assert_eq!(
            drained_file_empty_verdict(&path, &later),
            Err("durability_lag"),
            "the ack was stamped at the real clock, so the lag has not passed"
        );
        forget_resume_offset(&path);
    }

    /// A stamp a round finds confirmed is settled, so a LATER episode does
    /// not rewind (and re-send) bytes whose acks were already confirmed
    /// before it began; the live hour file can be gigabytes.
    ///
    /// Bite: remove the settle arm of `rewind_after_suspicion` and the last
    /// call rewinds.
    #[test]
    fn test_regression_da13b_a_stamp_confirmed_before_an_episode_is_never_rewound() {
        let path = temp_dir("da13b-settle").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        note_spill_ack(&path, &view, 0, 42);
        record_resume_offset(&path, 42);
        let confirmed = confirmed_spill_view(&view);
        assert!(!rewind_after_suspicion(&path, &confirmed, "ticks"));
        let mut after_episode = confirmed;
        after_episode.suspect_episodes += 1;
        assert!(
            !rewind_after_suspicion(&path, &after_episode, "ticks"),
            "acks confirmed before the episode are settled"
        );
        assert_eq!(resume_offset_for(&path), 42, "the offset is kept");
        assert_eq!(drained_file_empty_verdict(&path, &after_episode), Ok(()));
        forget_resume_offset(&path);
    }

    /// C4: the replay loop holds the whole round while the sink is suspect.
    /// Source scan, because the loop's sleep makes a live run impractical.
    #[test]
    fn test_regression_da13b_the_replay_loop_holds_while_the_sink_is_suspect() {
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("async fn run_replay_loop")
            .nth(1)
            .expect("run_replay_loop must exist");
        let view_at = body
            .find("SpillSinkView::current()")
            .expect("the loop takes a view each round");
        let hold_at = body
            .find("if held || held_suspect")
            .expect("the round is held while the sink is suspect");
        let call_at = body
            .find("replay_spill_dir(")
            .expect("the loop still replays");
        assert!(view_at < hold_at && hold_at < call_at);
        let mut suspect = test_spill_view();
        suspect.suspect = true;
        assert_eq!(
            drained_file_empty_verdict(Path::new("no-such-spill-file"), &suspect),
            Err("sink_suspect")
        );
    }

    /// C2 / C16 / split 1: a file that is not last is still live when the hour
    /// in its name has not passed, when it was written recently, when its
    /// name does not parse, or when its hour is in the future.
    ///
    /// Bite: make `spill_file_may_still_be_appended` return `is_last` and
    /// every non-last "live" assertion below fails.
    #[test]
    fn test_regression_da13b_a_non_last_file_of_the_current_hour_or_recently_written_is_live() {
        let dir = temp_dir("da13b-live");
        let now = test_spill_view();
        let real_now = SpillSinkView::current();
        let hour_now = now.now_unix_secs / SPILL_FILE_HOUR_SECS;
        let check = |name: String, view: &SpillSinkView| {
            let p = dir.join(name);
            std::fs::write(&p, GOOD_LINE).expect("write"); // APPROVED: test
            let meta = std::fs::metadata(&p).expect("meta"); // APPROVED: test
            spill_file_may_still_be_appended(&p, &meta, false, view, "ticks")
        };
        assert!(
            !check("ticks-dhan-1.ilp".into(), &now),
            "an old hour, quiet for two hours: closed"
        );
        assert!(
            check("ticks-dhan-2.ilp".into(), &real_now),
            "written just now: live, whatever its name says"
        );
        assert!(
            check(format!("ticks-dhan-{hour_now}.ilp"), &now),
            "the current hour (a clock stepped back across the hour): live"
        );
        assert!(
            check(format!("ticks-dhan-{}.ilp", hour_now + 5), &now),
            "a future hour (a clock that stepped forward): live"
        );
        assert!(
            check("ticks-dhan-x.ilp".into(), &now),
            "a name that does not parse: live"
        );
        let last = dir.join("ticks-dhan-3.ilp");
        std::fs::write(&last, GOOD_LINE).expect("write"); // APPROVED: test
        let meta = std::fs::metadata(&last).expect("meta"); // APPROVED: test
        assert!(spill_file_may_still_be_appended(
            &last, &meta, true, &now, "ticks"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C2 / C16, end to end: the writer still appends to the current hour's
    /// file while a future-named file sorts last. It is never emptied.
    #[tokio::test]
    async fn test_regression_da13b_the_current_hour_file_is_never_emptied_when_not_last() {
        let dir = temp_dir("da13b-step");
        let view = test_spill_view();
        let hour_now = view.now_unix_secs / SPILL_FILE_HOUR_SECS;
        let current = dir.join(format!("ticks-dhan-{hour_now}.ilp"));
        let future = dir.join(format!("ticks-dhan-{}.ilp", hour_now + 3));
        std::fs::write(&current, GOOD_LINE).expect("write"); // APPROVED: test
        std::fs::write(&future, GOOD_LINE).expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let _ = replay_spill_dir(&dir, &url, &client, &view).await;
        let _ = replay_spill_dir(&dir, &url, &client, &confirmed_spill_view(&view)).await;
        server.abort();
        assert_eq!(
            std::fs::metadata(&current).expect("m").len(),
            GOOD_LINE.len() as u64,
            "a file the writer may still append to is never emptied"
        );
        forget_resume_offset(&current);
        forget_resume_offset(&future);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C3 / C12: the last line of the live file without a newline is a line
    /// still being written. It is not POSTed, and the resume offset stays on
    /// the last newline, so the next round sends the whole line once.
    ///
    /// Bite: drop the live-file arm of the `None if eof` match and round 1
    /// POSTs the half line and resumes at the snapshot length.
    #[tokio::test]
    async fn test_regression_da13b_a_half_written_last_line_of_the_live_file_is_not_posted() {
        let dir = temp_dir("da13b-half");
        let live = dir.join("ticks-dhan-1.ilp");
        let half: &[u8] = b"ticks,segment=NSE_EQ,feed=dhan secu";
        let rest: &[u8] = b"rity_id=3i 3\n";
        let mut body = GOOD_LINE.to_vec();
        body.extend_from_slice(GOOD_LINE);
        body.extend_from_slice(half);
        std::fs::write(&live, &body).expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        // The clock at the real time: the file was written just now, so its
        // half-written last line is still being written, not torn for good.
        let mut view = test_spill_view();
        view.now_unix_secs = SpillSinkView::current().now_unix_secs;
        let whole = 2 * GOOD_LINE.len() as u64;

        let first = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(first.bytes_replayed, whole, "only whole lines are sent");
        assert_eq!(first.lines_set_aside, 0);
        assert_eq!(
            resume_offset_for(&live),
            whole,
            "resume on the last newline"
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .expect("open"); // APPROVED: test
        std::io::Write::write_all(&mut f, rest).expect("append"); // APPROVED: test
        drop(f);
        let second = replay_spill_dir(&dir, &url, &client, &view).await;
        server.abort();
        assert_eq!(
            second.bytes_replayed,
            (half.len() + rest.len()) as u64,
            "the completed line is sent whole, once"
        );
        assert_eq!(second.lines_set_aside, 0);
        forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C6: a file drained while it was the live one is emptied once its hour
    /// has closed. Before, the fully-drained skip ran first and it never was.
    ///
    /// Bite: move the `resume_from == file_len` skip back above the empty-out.
    #[tokio::test]
    async fn test_regression_da13b_a_file_drained_while_live_is_emptied_after_its_hour_closes() {
        let dir = temp_dir("da13b-c6");
        let first_file = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&first_file, GOOD_LINE).expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let view = test_spill_view();
        let _ = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(resume_offset_for(&first_file), GOOD_LINE.len() as u64);
        // The hour rolls over: a new file is now the last.
        let next = dir.join("ticks-dhan-2.ilp");
        std::fs::write(&next, GOOD_LINE).expect("write"); // APPROVED: test
        let _ = replay_spill_dir(&dir, &url, &client, &confirmed_spill_view(&view)).await;
        server.abort();
        assert_eq!(
            std::fs::metadata(&first_file).expect("m").len(),
            0,
            "a fully drained closed file must be emptied, not held for the life of the process"
        );
        forget_resume_offset(&next);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C10: a spill file the replay cannot stat, open, seek or re-check is
    /// counted on the replay-failed counter with a coded error. Source scan:
    /// running as root, a test cannot make an existing file unreadable.
    ///
    /// Bite: remove any one `note_spill_file_unreadable` call.
    #[test]
    fn test_regression_da13b_every_unreadable_branch_is_counted() {
        let tick = include_str!("tick_spill_replay.rs");
        let tick_fn = tick
            .split("pub async fn replay_spill_dir(")
            .nth(1)
            .expect("replay_spill_dir");
        let tick_fn = &tick_fn[..tick_fn.find("\n\u{7d}\n").unwrap_or(tick_fn.len())];
        for source in [
            "spill_stat_failed",
            "spill_open_failed",
            "spill_seek_failed",
            "spill_recheck_failed",
        ] {
            assert!(
                tick_fn.contains(&format!("note_spill_file_unreadable(&path, \"{source}\"")),
                "replay_spill_dir must count and log `{source}`"
            );
        }
        let depth = include_str!("depth_spill_frame.rs");
        let depth_fn = depth
            .split("pub async fn replay_framed_spill_dir(")
            .nth(1)
            .expect("replay_framed_spill_dir");
        let depth_fn = &depth_fn[..depth_fn.find("\n\u{7d}\n").unwrap_or(depth_fn.len())];
        for source in [
            "spill_stat_failed",
            "spill_open_failed",
            "spill_recheck_failed",
        ] {
            assert!(
                depth_fn.contains(&format!("note_spill_file_unreadable(&path, \"{source}\"")),
                "replay_framed_spill_dir must count and log `{source}`"
            );
        }
        let helper = tick
            .split("pub(crate) fn note_spill_file_unreadable")
            .nth(1)
            .expect("helper");
        let helper = &helper[..helper.find("\n\u{7d}\n").unwrap_or(helper.len())];
        assert!(helper.contains("tv_tick_spill_replay_failed_total"));
        assert!(helper.contains("error!("));
        assert!(helper.contains("code = ErrorCode::"));
    }

    /// C11: when a later chunk is refused whole and the file is quarantined,
    /// the chunks QuestDB already accepted this round are counted as
    /// replayed, not reported as missing.
    ///
    /// Bite: drop the `landed_this_round` increment and `bytes_replayed`
    /// reads 0.
    #[tokio::test]
    async fn test_regression_da13b_a_whole_file_quarantine_counts_the_chunks_already_accepted() {
        let dir = temp_dir("da13b-c11");
        let path = dir.join("ticks-dhan-1.ilp");
        // Whole good lines up to just under one chunk, then a poison line long
        // enough to cross the chunk edge, so the first chunk is all good and
        // the second is all poison (refused whole).
        let mut body: Vec<u8> = Vec::with_capacity(REPLAY_MAX_CHUNK_BYTES + 1024);
        let mut sid: u64 = 0;
        loop {
            sid += 1;
            let line = format!("ticks,segment=NSE_EQ,feed=dhan security_id={sid}i 1\n");
            if body.len() + line.len() > REPLAY_MAX_CHUNK_BYTES - 64 {
                break;
            }
            body.extend_from_slice(line.as_bytes());
        }
        let good = body.len() as u64;
        body.extend_from_slice(format!("poison {}\n", "p".repeat(256)).as_bytes());
        // More refused lines than isolation may set aside, so the chunk is
        // refused WHOLE although the first chunk was accepted.
        for n in 0..=REPLAY_MAX_REJECTED_LINES_PER_CHUNK {
            body.extend_from_slice(format!("poison {n}\n").as_bytes());
        }
        std::fs::write(&path, &body).expect("write"); // APPROVED: test
        let (url, server) = spawn_full_body_server(false).await;
        let client = crate::http_client::build_probe_client(30).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();
        assert_eq!(out.files_quarantined, 1);
        assert_eq!(
            out.bytes_replayed, good,
            "the accepted first chunk is in the database and is counted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Audit DA-13b review (2026-10-13): the rewind and the accounting ---

    fn rejected_lines_of(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(
            dir.join(QUARANTINE_DIR)
                .join(format!("{name}{REJECTED_LINES_SUFFIX}")),
        )
        .unwrap_or_default()
    }

    /// Review finding (HIGH): a rewind after a suspicion episode re-sends
    /// only the bytes whose acks were still unconfirmed, never the confirmed
    /// prefix. A line refused again on the re-send is kept again (2026-10-13
    /// (DA-13b review): a duplicate quarantine line is harmless, a dropped
    /// row is not), but the confirmed prefix's refused line is not.
    ///
    /// Bite: rewind to 0 and round 4 re-sends the prefix (bytes) and keeps
    /// poison A a second time.
    #[tokio::test]
    async fn test_regression_da13b_a_rewind_resends_only_the_unconfirmed_range_once() {
        let dir = temp_dir("da13b-rewind-range");
        let live = dir.join("ticks-dhan-1.ilp");
        let poison_a: &[u8] = b"ticks,poison=a 1\n";
        let poison_b: &[u8] = b"ticks,poison=b 2\n";
        let mut part_a = GOOD_LINE.to_vec();
        part_a.extend_from_slice(poison_a);
        let mut part_b = GOOD_LINE.to_vec();
        part_b.extend_from_slice(poison_b);
        part_b.extend_from_slice(GOOD_LINE);
        std::fs::write(&live, &part_a).expect("write"); // APPROVED: test
        let (url, server) = spawn_full_body_server(false).await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test

        // Round 1 drains part A (one line set aside); round 2 confirms it.
        let view = test_spill_view();
        let r1 = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(r1.lines_set_aside, 1);
        let confirmed = confirmed_spill_view(&view);
        let _ = replay_spill_dir(&dir, &url, &client, &confirmed).await;

        // The writer appends part B; round 3 drains it (one more set aside).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .expect("open"); // APPROVED: test
        std::io::Write::write_all(&mut f, &part_b).expect("append"); // APPROVED: test
        drop(f);
        let r3 = replay_spill_dir(&dir, &url, &client, &confirmed).await;
        assert_eq!(r3.lines_set_aside, 1);

        // An episode begins before part B's acks are confirmed.
        let mut after_episode = confirmed_spill_view(&confirmed);
        after_episode.suspect_episodes += 1;
        let r4 = replay_spill_dir(&dir, &url, &client, &after_episode).await;
        server.abort();
        assert_eq!(
            r4.bytes_replayed,
            2 * GOOD_LINE.len() as u64,
            "only part B's good lines are sent again, not part A"
        );
        assert_eq!(r4.lines_set_aside, 1, "the re-sent refusal is kept again");
        let mut expected = poison_a.to_vec();
        expected.extend_from_slice(poison_b);
        expected.extend_from_slice(poison_b);
        assert_eq!(
            rejected_lines_of(&dir, "ticks-dhan-1.ilp"),
            expected,
            "poison A once (confirmed prefix), poison B once per pass"
        );
        forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding (MED): the torn last line of a CLOSED file is never
    /// POSTed, even when QuestDB would accept it (a line cut before its
    /// timestamp is valid ILP and would be stored at the server's time). It
    /// is set aside and counted.
    ///
    /// Bite: POST the tail (`None if eof => filled`) and `bytes_replayed`
    /// includes it while nothing is set aside.
    #[tokio::test]
    async fn test_regression_da13b_a_torn_tail_of_a_closed_file_is_never_posted() {
        let dir = temp_dir("da13b-torn-closed");
        let closed = dir.join("ticks-dhan-1.ilp");
        let torn: &[u8] = b"ticks,segment=NSE_EQ,feed=dhan security_id=3i";
        let mut body = GOOD_LINE.to_vec();
        body.extend_from_slice(torn);
        std::fs::write(&closed, &body).expect("write"); // APPROVED: test
        std::fs::write(dir.join("ticks-dhan-2.ilp"), GOOD_LINE).expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();
        assert_eq!(
            out.bytes_replayed,
            2 * GOOD_LINE.len() as u64,
            "the torn fragment is not sent"
        );
        assert_eq!(out.lines_set_aside, 1);
        let mut kept = torn.to_vec();
        kept.push(b'\n');
        assert_eq!(rejected_lines_of(&dir, "ticks-dhan-1.ilp"), kept);
        assert_eq!(resume_offset_for(&closed), body.len() as u64);
        forget_resume_offset(&closed);
        forget_resume_offset(&dir.join("ticks-dhan-2.ilp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding (LOW): a live file whose only new bytes are a line
    /// still being written made no progress, so it is not counted as
    /// replayed (it read as a recovery every 300 s).
    ///
    /// Bite: count `files_replayed` when `accepted == 0`.
    #[tokio::test]
    async fn test_regression_da13b_a_live_file_with_only_a_partial_line_is_not_counted() {
        let dir = temp_dir("da13b-partial-count");
        let live = dir.join("ticks-dhan-1.ilp");
        let mut body = GOOD_LINE.to_vec();
        body.extend_from_slice(b"ticks,segment=NSE_EQ,feed=dhan secu");
        std::fs::write(&live, &body).expect("write"); // APPROVED: test
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        // The clock at the real time: the file was written just now, so its
        // partial last line is still being written.
        let mut view = test_spill_view();
        view.now_unix_secs = SpillSinkView::current().now_unix_secs;
        let first = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(first.files_replayed, 1);
        let second = replay_spill_dir(&dir, &url, &client, &view).await;
        server.abort();
        assert_eq!(second.files_replayed, 0, "no progress, no count");
        assert_eq!(second.bytes_replayed, 0);
        forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review findings (MED): a quarantine reports how much is CONFIRMED in
    /// the database (not merely acknowledged), and the length as it is when
    /// the file is moved (a live file grows while it is read).
    ///
    /// Bite: report `resume_from` as confirmed, or the round-start length.
    #[test]
    fn test_regression_da13b_a_quarantine_reports_confirmed_rows_and_the_current_length() {
        let dir = temp_dir("da13b-report");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&path, vec![b'x'; 300]).expect("write"); // APPROVED: test
        let view = test_spill_view();
        // Bytes 100..200 were acknowledged last round and are not confirmed.
        note_spill_ack(&path, &view, 100, 200);
        let report = quarantine_report(&path, &view, 200, 50, 250);
        assert_eq!(report.confirmed_through, 100);
        assert_eq!(report.acked_through, 250);
        assert_eq!(report.len_now, 300, "re-read now, not the 250 at the start");
        assert_eq!(report.unreplayed_bytes(), 200);
        // Once confirmed, everything before the round's start is confirmed.
        let confirmed = confirmed_spill_view(&view);
        let report = quarantine_report(&path, &confirmed, 200, 50, 250);
        assert_eq!(report.confirmed_through, 200);
        forget_resume_offset(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding (LOW): the over-long-line quarantine counts the chunks
    /// already accepted, like the whole-file refusal.
    ///
    /// Bite: drop the `landed_this_round` increment in that branch.
    #[tokio::test]
    async fn test_regression_da13b_an_over_long_line_quarantine_counts_accepted_chunks() {
        let dir = temp_dir("da13b-over-long");
        let path = dir.join("ticks-dhan-1.ilp");
        let mut body = GOOD_LINE.to_vec();
        body.extend_from_slice(GOOD_LINE);
        body.resize(body.len() + REPLAY_STREAM_BUFFER_BYTES + 1024, b'x');
        std::fs::write(&path, &body).expect("write"); // APPROVED: test
        drop(body);
        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();
        assert_eq!(out.files_quarantined, 1);
        assert_eq!(out.bytes_replayed, 2 * GOOD_LINE.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding (LOW): the view reads the episode count before AND
    /// after the suspect flag, and reads "suspect" when they differ, so an
    /// episode beginning mid-read can never yield "not suspect" with the new
    /// count. Source scan: the race is nanoseconds wide.
    ///
    /// Bite: read the count after the flag only.
    #[test]
    fn test_regression_da13b_the_view_reads_the_episode_count_around_the_suspect_flag() {
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("pub fn current() -> Self {")
            .nth(1)
            .expect("current()");
        let body = &body[..body.find("\n    }\n").unwrap_or(body.len())];
        let before = body
            .find("let episodes_before = wm.suspect_episode_count();")
            .expect("count read first");
        let flag = body.find("wm.is_sink_suspect()").expect("flag read");
        let after = body
            .find("let episodes_after = wm.suspect_episode_count();")
            .expect("count read again");
        assert!(before < flag && flag < after);
        assert!(body.contains("suspect || episodes_before != episodes_after"));
    }

    // --- DA-13b review, round 2 (2026-10-13): C1 to C7 ---

    /// Counts one counter name, every label set summed, through a recorder
    /// scoped with `metrics::with_local_recorder` (never a global install, so
    /// parallel tests cannot steal each other's counts). Same shape as the
    /// one in `seal_spill`'s tests; shared with `depth_spill_frame`'s tests.
    #[derive(Clone)]
    pub(crate) struct OneCounterRecorder {
        name: &'static str,
        total: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    struct OneCounter {
        total: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl metrics::CounterFn for OneCounter {
        fn increment(&self, value: u64) {
            self.total
                .fetch_add(value, std::sync::atomic::Ordering::Relaxed);
        }
        fn absolute(&self, _: u64) {}
    }

    impl metrics::Recorder for OneCounterRecorder {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            if key.name() == self.name {
                metrics::Counter::from_arc(std::sync::Arc::new(OneCounter {
                    total: std::sync::Arc::clone(&self.total),
                }))
            } else {
                metrics::Counter::noop()
            }
        }
        fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::noop()
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    impl OneCounterRecorder {
        pub(crate) fn new(name: &'static str) -> Self {
            Self {
                name,
                total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }
        }

        /// Runs `fut` to completion on `rt`, on THIS thread, with this
        /// recorder installed for this thread only, and returns what it
        /// returned and how much the counter moved. Tasks the test spawned
        /// on `rt` (a test server) run on its worker threads.
        // TEST-EXEMPT: test-only helper (inside #[cfg(test)], shared with depth_spill_frame's tests)
        pub(crate) fn count_during<F: std::future::Future>(
            &self,
            rt: &tokio::runtime::Runtime,
            fut: F,
        ) -> (F::Output, u64) {
            let before = self.total.load(std::sync::atomic::Ordering::Relaxed);
            let out = metrics::with_local_recorder(self, || rt.block_on(fut));
            let after = self.total.load(std::sync::atomic::Ordering::Relaxed);
            (out, after.saturating_sub(before))
        }
    }

    /// A multi-thread runtime for the counter tests: `block_on` runs the
    /// replay on the calling thread (where the local recorder is), the test
    /// server on the workers.
    pub(crate) fn counter_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime") // APPROVED: test
    }

    /// A test server that answers 204 to everything until `refuse` is set,
    /// then 400 to any body holding `flip`. Models the C1 shape: the first
    /// pass's 2xx (from a suspended table) for a row QuestDB refuses on the
    /// re-send. Shared with `depth_spill_frame`'s tests.
    pub(crate) async fn spawn_flip_server(
        refuse: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind"); // APPROVED: test
        let addr = listener.local_addr().expect("addr"); // APPROVED: test
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req: Vec<u8> = Vec::new();
                let mut buf = vec![0_u8; 65536];
                let mut body_start = None;
                let mut content_len = 0_usize;
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0); // APPROVED: test
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if body_start.is_none()
                        && let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        body_start = Some(i + 4);
                        let head = String::from_utf8_lossy(&req[..i]).to_ascii_lowercase();
                        content_len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                    if let Some(start) = body_start
                        && req.len() >= start + content_len
                    {
                        break;
                    }
                }
                let body = &req[body_start.unwrap_or(req.len()).min(req.len())..];
                let flip = body.windows(4).any(|w| w == b"flip");
                let resp = if flip && refuse.load(std::sync::atomic::Ordering::SeqCst) {
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/write"), handle)
    }

    /// C1 (tick): the first pass got a 2xx for every line (from a suspended
    /// table); after the episode the re-send is refused for one line. That
    /// line is the only copy of the row and must be kept in quarantine and
    /// counted, although it lies below `resend_through`.
    ///
    /// Bite: put back the `if repeat { ...; continue; }` skip in the
    /// isolated arm and round 2 keeps nothing, then empties the file.
    #[tokio::test]
    async fn test_regression_da13b_a_repeat_refusal_is_set_aside() {
        let dir = temp_dir("da13b-c1-tick");
        let closed = dir.join("ticks-dhan-1.ilp");
        let flip: &[u8] = b"ticks,flip=1 security_id=9i 1\n";
        let mut body = GOOD_LINE.to_vec();
        body.extend_from_slice(flip);
        std::fs::write(&closed, &body).expect("write"); // APPROVED: test
        std::fs::write(dir.join("ticks-dhan-2.ilp"), b"").expect("write"); // APPROVED: test
        let refuse = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (url, server) = spawn_flip_server(std::sync::Arc::clone(&refuse)).await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test

        let view = test_spill_view();
        let first = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(first.bytes_replayed, body.len() as u64, "all acked");
        assert_eq!(first.lines_set_aside, 0);

        refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut after_episode = confirmed_spill_view(&view);
        after_episode.suspect_episodes += 1;
        let second = replay_spill_dir(&dir, &url, &client, &after_episode).await;
        assert_eq!(second.lines_set_aside, 1, "the refused re-send is kept");
        assert_eq!(rejected_lines_of(&dir, "ticks-dhan-1.ilp"), flip);

        // Confirmed: the file is emptied, and the row survives in quarantine.
        let _ = replay_spill_dir(&dir, &url, &client, &confirmed_spill_view(&after_episode)).await;
        server.abort();
        assert_eq!(std::fs::metadata(&closed).expect("m").len(), 0);
        assert_eq!(rejected_lines_of(&dir, "ticks-dhan-1.ilp"), flip);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C1 (tick): lines the window filter refuses on a re-send are counted
    /// too, never dropped without a count.
    ///
    /// Bite: put back `&& !repeat` on the out-of-window count and round 2
    /// counts 0.
    #[test]
    fn test_regression_da13b_repeat_out_of_window_lines_are_counted() {
        let dir = temp_dir("da13b-c1-window");
        let closed = dir.join("ticks-dhan-1.ilp");
        let mut body = tick_line(12 * 3600).into_bytes();
        body.extend_from_slice(tick_line(22 * 3600).as_bytes());
        std::fs::write(&closed, &body).expect("write"); // APPROVED: test
        std::fs::write(dir.join("ticks-dhan-2.ilp"), b"").expect("write"); // APPROVED: test
        let rt = counter_test_runtime();
        let (url, server) = rt.block_on(spawn_selective_server());
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let rec = OneCounterRecorder::new("tv_spill_replay_rows_out_of_window_refused_total");
        let view = test_spill_view();
        let (_, first) = rec.count_during(&rt, replay_spill_dir(&dir, &url, &client, &view));
        assert_eq!(first, 1);
        let mut after_episode = confirmed_spill_view(&view);
        after_episode.suspect_episodes += 1;
        let (second, counted) =
            rec.count_during(&rt, replay_spill_dir(&dir, &url, &client, &after_episode));
        server.abort();
        assert!(second.bytes_replayed > 0, "the file was sent again");
        assert_eq!(counted, 1, "the re-sent out-of-window line is counted");
        forget_resume_offset(&closed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C2: a non-last file whose hour ended less than the quiet period ago
    /// is still live, even when its last write is long past: a rescue that
    /// read the clock in that hour may still be opening it.
    ///
    /// Bite: drop the `+ SPILL_LIVE_FILE_QUIET_SECS` on the hour end and the
    /// first assert fails.
    #[test]
    fn test_regression_da13b_a_file_is_closed_only_a_quiet_period_after_its_hour() {
        let dir = temp_dir("da13b-c2");
        let real = SpillSinkView::current();
        // An hour well ahead of the real clock, so the file's real mtime is
        // hours before the view's instant (quiet by mtime).
        let hour = real.now_unix_secs / SPILL_FILE_HOUR_SECS + 5;
        let path = dir.join(format!("ticks-dhan-{hour}.ilp"));
        std::fs::write(&path, GOOD_LINE).expect("write"); // APPROVED: test
        let meta = std::fs::metadata(&path).expect("meta"); // APPROVED: test
        let hour_end = (hour + 1) * SPILL_FILE_HOUR_SECS;
        let mut view = test_spill_view();
        view.now_unix_secs = hour_end + SPILL_LIVE_FILE_QUIET_SECS - 1;
        assert!(
            !spill_file_closed_and_quiet(&path, &meta, &view),
            "the hour ended under the quiet period ago: still live"
        );
        assert!(spill_file_may_still_be_appended(
            &path, &meta, false, &view, "ticks"
        ));
        view.now_unix_secs = hour_end + SPILL_LIVE_FILE_QUIET_SECS;
        assert!(spill_file_closed_and_quiet(&path, &meta, &view));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C3: the rewind lowers the offset BEFORE it clears the acks, and the
    /// archive's check reads the offset on both sides of the acks, so no
    /// interleaving reads a drained file with nothing unconfirmed. The race
    /// is a few instructions wide, so the order is pinned by a source scan
    /// and the mid-rewind state by a direct check.
    ///
    /// Bite: clear `pending` before `rewind_resume_offset`, or drop either
    /// offset read in `spill_file_pending`.
    #[test]
    fn test_regression_da13b_a_rewind_never_reads_drained_with_nothing_pending() {
        let src = include_str!("tick_spill_replay.rs");
        let rewind = src
            .split("pub(crate) fn rewind_after_suspicion(")
            .nth(1)
            .expect("rewind_after_suspicion");
        let rewind = &rewind[..rewind.find("\n\u{7d}\n").unwrap_or(rewind.len())];
        let lowered = rewind
            .find("rewind_resume_offset(path, rewind_to);")
            .expect("the rewind lowers the offset");
        let cleared = rewind
            .rfind("state.pending = None;")
            .expect("the rewind clears the acks");
        assert!(
            lowered < cleared,
            "offset lowered before the acks are cleared"
        );
        let helper = src
            .split("pub(crate) fn spill_file_pending(")
            .nth(1)
            .expect("spill_file_pending");
        let helper = &helper[..helper.find("\n\u{7d}\n").unwrap_or(helper.len())];
        let first = helper.find("resume_offset_for(path)").expect("offset read");
        let ack = helper.find("has_unconfirmed_ack(path)").expect("ack read");
        let again = helper
            .rfind("resume_offset_for(path)")
            .expect("offset re-read");
        assert!(first < ack && ack < again, "offset, acks, offset");
        let archive = include_str!("partition_archive.rs");
        assert!(archive.contains("crate::tick_spill_replay::spill_file_pending(&entry.path()"));

        // The state between the rewind's two steps (offset lowered, acks
        // still pending) and after both read pending; a drained file with
        // nothing pending does not.
        let path = temp_dir("da13b-c3").join("depth-dhan-1.dspl");
        let view = test_spill_view();
        note_spill_ack(&path, &view, 10, 42);
        record_resume_offset(&path, 42);
        rewind_resume_offset(&path, 10);
        assert!(spill_file_pending(&path, 42), "mid-rewind: pending");
        let mut after_episode = view;
        after_episode.suspect_episodes += 1;
        record_resume_offset(&path, 42);
        assert!(rewind_after_suspicion(&path, &after_episode, "depth"));
        assert!(spill_file_pending(&path, 42), "rewound: pending");
        forget_resume_offset(&path);
        record_resume_offset(&path, 42);
        assert!(!spill_file_pending(&path, 42), "drained, nothing pending");
        forget_resume_offset(&path);
    }

    /// C4: confirmation is sticky. Acks a check has once seen confirmed are
    /// settled, so a later run of suspicion episodes (more than one, which
    /// `confirmed_at` alone reads as doubt) never rewinds them, from any of
    /// the three places that can see the confirmation first.
    ///
    /// Bite: make any of `drained_file_empty_verdict`,
    /// `has_unconfirmed_ack_at` or `confirmed_through_for` read the acks
    /// without settling them, and its file is rewound.
    #[test]
    fn test_regression_da13b_confirmation_is_sticky() {
        let dir = temp_dir("da13b-c4");
        let view = test_spill_view();
        let confirmed = confirmed_spill_view(&view);
        let mut two_episodes = confirmed;
        two_episodes.suspect_episodes += 2;
        let paths: Vec<PathBuf> = (1..=3)
            .map(|n| dir.join(format!("ticks-dhan-{n}.ilp")))
            .collect();
        for path in &paths {
            note_spill_ack(path, &view, 0, 42);
            record_resume_offset(path, 42);
        }
        assert_eq!(drained_file_empty_verdict(&paths[0], &confirmed), Ok(()));
        assert!(!has_unconfirmed_ack_at(&paths[1], &confirmed));
        assert_eq!(confirmed_through_for(&paths[2], &confirmed, 42), 42);
        for path in &paths {
            assert!(
                !rewind_after_suspicion(path, &two_episodes, "ticks"),
                "{}: settled once seen confirmed, never rewound",
                path.display()
            );
            assert_eq!(resume_offset_for(path), 42);
            assert!(!has_unconfirmed_ack_at(path, &two_episodes));
            forget_resume_offset(path);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A test server that deletes `victim` when it receives the first POST,
    /// then answers 204, so the round's size re-check after the drain fails.
    async fn spawn_deleting_server(victim: PathBuf) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind"); // APPROVED: test
        let addr = listener.local_addr().expect("addr"); // APPROVED: test
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0_u8; 65536];
                let _ = sock.read(&mut buf).await;
                let _ = std::fs::remove_file(&victim);
                let resp =
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/write"), handle)
    }

    /// C5: a size re-check that fails after QuestDB accepted the drain still
    /// counts the accepted bytes. The offset is kept, so the next round never
    /// re-sends (or counts) them.
    ///
    /// Bite: drop the `bytes_replayed` increment in the re-check's `Err` arm
    /// and `bytes_replayed` reads 0.
    #[tokio::test]
    async fn test_regression_da13b_a_failed_recheck_counts_the_accepted_bytes() {
        let dir = temp_dir("da13b-c5");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&path, GOOD_LINE).expect("write"); // APPROVED: test
        let (url, server) = spawn_deleting_server(path.clone()).await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client, &test_spill_view()).await;
        server.abort();
        assert_eq!(out.files_failed, 1, "the re-check failed");
        assert_eq!(out.bytes_replayed, GOOD_LINE.len() as u64);
        assert_eq!(resume_offset_for(&path), GOOD_LINE.len() as u64);
        forget_resume_offset(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C6: a round that only set a torn tail aside sent nothing, so it is not
    /// counted (or logged) as a recovery, on a closed file or on the last.
    ///
    /// Bite: count `files_replayed` on `accepted > 0` instead of the bytes
    /// QuestDB took, and `files_replayed` reads 2.
    #[tokio::test]
    async fn test_regression_da13b_a_set_aside_only_round_is_not_a_recovery() {
        let dir = temp_dir("da13b-c6-torn");
        let torn: &[u8] = b"ticks,segment=NSE_EQ,feed=dhan security_id=3i";
        std::fs::write(dir.join("ticks-dhan-1.ilp"), torn).expect("write"); // APPROVED: test
        std::fs::write(dir.join("ticks-dhan-2.ilp"), torn).expect("write"); // APPROVED: test
        // Nothing is POSTed: an address nobody listens on.
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(
            &dir,
            "http://127.0.0.1:9/write",
            &client,
            &test_spill_view(),
        )
        .await;
        assert_eq!(out.lines_set_aside, 2);
        assert_eq!(out.bytes_replayed, 0);
        assert_eq!(out.files_replayed, 0, "nothing reached the database");
        forget_resume_offset(&dir.join("ticks-dhan-1.ilp"));
        forget_resume_offset(&dir.join("ticks-dhan-2.ilp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C7: a torn tail set aside moves the PAGED rejected-lines counter, like
    /// any other row kept out of the database.
    ///
    /// Bite: drop the `REPLAY_LINES_REJECTED_COUNTER` increment in the torn
    /// tail arm and the count reads 0.
    #[test]
    fn test_regression_da13b_a_torn_tail_is_paged() {
        let dir = temp_dir("da13b-c7-tick");
        let closed = dir.join("ticks-dhan-1.ilp");
        let mut body = GOOD_LINE.to_vec();
        body.extend_from_slice(b"ticks,segment=NSE_EQ,feed=dhan security_id=3i");
        std::fs::write(&closed, &body).expect("write"); // APPROVED: test
        std::fs::write(dir.join("ticks-dhan-2.ilp"), b"").expect("write"); // APPROVED: test
        let rt = counter_test_runtime();
        let (url, server) = rt.block_on(spawn_selective_server());
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let rec = OneCounterRecorder::new(REPLAY_LINES_REJECTED_COUNTER);
        let (out, paged) = rec.count_during(
            &rt,
            replay_spill_dir(&dir, &url, &client, &test_spill_view()),
        );
        server.abort();
        assert_eq!(out.lines_set_aside, 1);
        assert_eq!(paged, 1, "the torn tail is on the paged counter");
        forget_resume_offset(&closed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- DA-13b: direct tests for the spill-replay helpers -------------------

    #[test]
    fn test_note_spill_ack_opens_and_joins_one_pending_range() {
        let path = temp_dir("da13b-direct-ack").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        assert!(!has_unconfirmed_ack_at(&path, &view), "no ack yet");
        note_spill_ack(&path, &view, 10, 20);
        note_spill_ack(&path, &view, 20, 42);
        assert!(has_unconfirmed_ack_at(&path, &view), "acks pending");
        // The joined range kept the first ack's start: a rewind returns to 10.
        record_resume_offset(&path, 42);
        let mut after_episode = view;
        after_episode.suspect_episodes += 1;
        assert!(rewind_after_suspicion(&path, &after_episode, "ticks"));
        assert_eq!(resume_offset_for(&path), 10);
        forget_resume_offset(&path);
    }

    #[test]
    fn test_rewind_after_suspicion_rewinds_only_unconfirmed_acks_after_an_episode() {
        let path = temp_dir("da13b-direct-rewind").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        assert!(
            !rewind_after_suspicion(&path, &view, "ticks"),
            "no acks: nothing to rewind"
        );
        note_spill_ack(&path, &view, 0, 30);
        record_resume_offset(&path, 30);
        assert!(
            !rewind_after_suspicion(&path, &view, "ticks"),
            "same episode: no rewind"
        );
        let confirmed = confirmed_spill_view(&view);
        assert!(
            !rewind_after_suspicion(&path, &confirmed, "ticks"),
            "confirmed acks are settled, not rewound"
        );
        let mut after_episode = confirmed;
        after_episode.suspect_episodes += 2;
        assert!(
            !rewind_after_suspicion(&path, &after_episode, "ticks"),
            "settled acks stay settled after later episodes"
        );
        assert_eq!(resume_offset_for(&path), 30);
        forget_resume_offset(&path);
    }

    #[test]
    fn test_resend_through_for_is_zero_until_a_rewind_marks_the_old_offset() {
        let path = temp_dir("da13b-direct-resend").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        assert_eq!(resend_through_for(&path), 0, "unknown file");
        note_spill_ack(&path, &view, 5, 50);
        record_resume_offset(&path, 50);
        assert_eq!(resend_through_for(&path), 0, "no rewind yet");
        let mut after_episode = view;
        after_episode.suspect_episodes += 1;
        assert!(rewind_after_suspicion(&path, &after_episode, "ticks"));
        assert_eq!(resend_through_for(&path), 50, "old offset dealt with");
        forget_resume_offset(&path);
        assert_eq!(resend_through_for(&path), 0, "forgotten with the offset");
    }

    #[test]
    fn test_spill_file_pending_reads_unsent_bytes_and_unconfirmed_acks() {
        let path = temp_dir("da13b-direct-pending").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        assert!(!spill_file_pending(&path, 0), "empty file");
        assert!(spill_file_pending(&path, 10), "bytes past the offset");
        record_resume_offset(&path, 10);
        assert!(!spill_file_pending(&path, 10), "drained, no acks");
        // The ack is stamped two hours ahead of the real clock, so the real
        // view cannot have confirmed it yet: the drained file stays pending.
        note_spill_ack(&path, &view, 0, 10);
        assert!(spill_file_pending(&path, 10), "drained, ack unconfirmed");
        forget_resume_offset(&path);
    }

    #[test]
    fn test_drained_file_empty_verdict_waits_for_confirmation() {
        let path = temp_dir("da13b-direct-verdict").join("ticks-dhan-1.ilp");
        let view = test_spill_view();
        assert_eq!(drained_file_empty_verdict(&path, &view), Ok(()), "no acks");
        let mut suspect = view;
        suspect.suspect = true;
        assert_eq!(
            drained_file_empty_verdict(&path, &suspect),
            Err(EMPTY_DEFER_REASONS[0])
        );
        note_spill_ack(&path, &view, 0, 10);
        assert_eq!(
            drained_file_empty_verdict(&path, &view),
            Err(EMPTY_DEFER_REASONS[1]),
            "no clean probes since the ack"
        );
        assert_eq!(
            drained_file_empty_verdict(&path, &confirmed_spill_view(&view)),
            Ok(())
        );
        forget_resume_offset(&path);
    }

    #[test]
    fn test_spill_file_hour_parses_the_last_dash_field() {
        assert_eq!(
            spill_file_hour(Path::new("/x/ticks-dhan-493517.ilp")),
            Some(493_517)
        );
        assert_eq!(spill_file_hour(Path::new("/x/depth-dhan-7.dspl")), Some(7));
        assert_eq!(spill_file_hour(Path::new("/x/ticks-dhan-abc.ilp")), None);
        assert_eq!(spill_file_hour(Path::new("/x/ticks.ilp")), None);
    }

    #[test]
    fn test_spill_file_closed_and_quiet_needs_a_parsed_past_hour() {
        let dir = temp_dir("da13b-direct-closed");
        let closed = dir.join("ticks-dhan-1.ilp");
        let unnamed = dir.join("ticks-dhan-x.ilp");
        std::fs::write(&closed, GOOD_LINE).expect("write"); // APPROVED: test
        std::fs::write(&unnamed, GOOD_LINE).expect("write"); // APPROVED: test
        let view = test_spill_view();
        let meta = std::fs::metadata(&closed).expect("meta"); // APPROVED: test
        assert!(spill_file_closed_and_quiet(&closed, &meta, &view));
        let meta_unnamed = std::fs::metadata(&unnamed).expect("meta"); // APPROVED: test
        assert!(
            !spill_file_closed_and_quiet(&unnamed, &meta_unnamed, &view),
            "a name that does not parse reads as not closed"
        );
        let mut now = view;
        now.now_unix_secs = SpillSinkView::current().now_unix_secs;
        let current_hour = now.now_unix_secs / SPILL_FILE_HOUR_SECS;
        let live = dir.join(format!("ticks-dhan-{current_hour}.ilp"));
        std::fs::write(&live, GOOD_LINE).expect("write"); // APPROVED: test
        let meta_live = std::fs::metadata(&live).expect("meta"); // APPROVED: test
        assert!(
            !spill_file_closed_and_quiet(&live, &meta_live, &now),
            "the current hour is not closed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_spill_file_may_still_be_appended_last_always_live_and_counts_nonlast_live() {
        let dir = temp_dir("da13b-direct-appended");
        let closed = dir.join("ticks-dhan-1.ilp");
        let unnamed = dir.join("ticks-dhan-x.ilp");
        std::fs::write(&closed, GOOD_LINE).expect("write"); // APPROVED: test
        std::fs::write(&unnamed, GOOD_LINE).expect("write"); // APPROVED: test
        let meta = std::fs::metadata(&closed).expect("meta"); // APPROVED: test
        let meta_unnamed = std::fs::metadata(&unnamed).expect("meta"); // APPROVED: test
        let view = test_spill_view();
        let rec = OneCounterRecorder::new(SPILL_REPLAY_NONLAST_LIVE_COUNTER);
        let (last, closed_nonlast, unnamed_nonlast) = metrics::with_local_recorder(&rec, || {
            (
                spill_file_may_still_be_appended(&closed, &meta, true, &view, "ticks"),
                spill_file_may_still_be_appended(&closed, &meta, false, &view, "ticks"),
                spill_file_may_still_be_appended(&unnamed, &meta_unnamed, false, &view, "ticks"),
            )
        });
        assert!(last, "the last file may always be appended");
        assert!(!closed_nonlast, "a closed, quiet non-last file is not live");
        assert!(unnamed_nonlast, "doubt keeps the file live");
        assert_eq!(
            rec.total.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "only the non-last live file is counted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_empty_drained_closed_file_empties_only_once_confirmed() {
        let dir = temp_dir("da13b-direct-empty");
        let path = dir.join("ticks-dhan-1.ilp");
        std::fs::write(&path, GOOD_LINE).expect("write"); // APPROVED: test
        let view = test_spill_view();
        note_spill_ack(&path, &view, 0, GOOD_LINE.len() as u64);
        record_resume_offset(&path, GOOD_LINE.len() as u64);
        assert!(matches!(
            empty_drained_closed_file(&path, &view, "ticks"),
            EmptyOutcome::Deferred
        ));
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(), // APPROVED: test
            GOOD_LINE.len() as u64,
            "a deferred file keeps its bytes"
        );
        assert!(matches!(
            empty_drained_closed_file(&path, &confirmed_spill_view(&view), "ticks"),
            EmptyOutcome::Emptied
        ));
        assert_eq!(std::fs::metadata(&path).expect("meta").len(), 0); // APPROVED: test
        assert_eq!(resume_offset_for(&path), 0, "offset forgotten on empty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_note_spill_file_unreadable_counts_one_failure() {
        let rec = OneCounterRecorder::new("tv_tick_spill_replay_failed_total");
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        metrics::with_local_recorder(&rec, || {
            note_spill_file_unreadable(Path::new("/x/ticks-dhan-1.ilp"), "stat", &err);
        });
        assert_eq!(rec.total.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    // ---- top_volume spill plan item 5 (2026-10-13): the replay policy ------

    #[test]
    fn test_replay_policy_for_ticks_and_depth_is_unchanged() {
        let ticks = replay_policy_for_dir(Path::new(crate::tick_persistence::TICK_SPILL_DIR));
        assert_eq!(ticks.label, "ticks");
        assert_eq!(ticks.window, ReplayWindow::Exchange);
        assert_eq!(ticks.hold, ReplayHold::Never);
        assert!(!ticks.framed_depth);
        assert_eq!(ticks.max_file_age_secs, None);
        let depth = replay_policy_for_dir(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR));
        assert_eq!(depth.label, "depth");
        assert_eq!(depth.window, ReplayWindow::Arrival);
        assert_eq!(depth.hold, ReplayHold::InSession);
        assert!(
            depth.framed_depth,
            "only depth drains the framed .dspl files"
        );
        assert_eq!(depth.max_file_age_secs, None);
        // An unknown folder keeps the old default: exchange clock, no hold.
        let other = replay_policy_for_dir(Path::new("/tmp/whatever"));
        assert_eq!(other.label, "other");
        assert_eq!(other.window, ReplayWindow::Exchange);
        assert_eq!(other.hold, ReplayHold::Never);
        // The ceilings: top_volume has its own; the others are unchanged.
        assert_eq!(
            spill_max_bytes_for_dir(Path::new(
                crate::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR
            )),
            crate::top_volume_rank_persistence::top_volume_spill_max_bytes()
        );
    }

    #[test]
    fn test_top_volume_replay_holds_until_tables_are_keyed() {
        let tv = replay_policy_for_dir(Path::new(
            crate::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR,
        ));
        assert_eq!(tv.label, "top_volume", "it read `other` before 2026-10-13");
        assert_eq!(tv.window, ReplayWindow::None);
        assert_eq!(tv.hold, ReplayHold::KeyedThenInSession);
        assert!(!tv.framed_depth, "no pass over framed depth files");
        // Truth table of the hold.
        let noon = 12 * 3600;
        let evening = 17 * 3600;
        assert!(
            replay_held(tv.hold, evening, false),
            "unkeyed: held after the close"
        );
        assert!(replay_held(tv.hold, noon, false));
        assert!(
            replay_held(tv.hold, noon, true),
            "keyed: held in session (Q1)"
        );
        assert!(
            !replay_held(tv.hold, evening, true),
            "keyed, after 15:40: drains"
        );
        assert!(
            !replay_held(ReplayHold::Never, noon, false),
            "ticks are never held"
        );
        assert!(replay_held(ReplayHold::InSession, noon, false));
        assert!(!replay_held(ReplayHold::InSession, evening, false));
    }

    #[test]
    fn test_top_volume_replay_hold_window_matches_depth() {
        for secs in [
            0,
            8 * 3600 - 1,
            8 * 3600,
            12 * 3600,
            56_400 - 1,
            56_400,
            86_399,
        ] {
            assert_eq!(
                replay_held(ReplayHold::KeyedThenInSession, secs, true),
                replay_held(ReplayHold::InSession, secs, true),
                "keyed top_volume holds exactly when depth holds, at {secs}"
            );
        }
    }

    #[test]
    fn test_top_volume_replay_applies_no_window_filter() {
        // A 22:00 IST stamp is outside every exchange window: ticks drop it,
        // top_volume keeps it (Muhurat boards, E5).
        let mut chunk = tick_line(12 * 3600);
        chunk.push_str(&tick_line(22 * 3600));
        let (kept, dropped) = retain_lines_for_window(chunk.as_bytes(), ReplayWindow::None);
        assert_eq!(dropped, 0);
        assert_eq!(kept, chunk.as_bytes());
        let (_, dropped_ticks) = retain_lines_for_window(chunk.as_bytes(), ReplayWindow::Exchange);
        assert_eq!(dropped_ticks, 1);
        let src = include_str!("tick_spill_replay.rs");
        let body = src
            .split("pub async fn replay_spill_dir(")
            .nth(1)
            .expect("replay_spill_dir");
        assert!(
            body.contains("retain_lines_for_window(raw, replay_policy_for_dir(dir).window)"),
            "the filter must follow the directory's policy"
        );
    }

    #[test]
    fn test_top_volume_spawn_sets_the_age_limit_from_the_archive_window() {
        let mut cfg = tickvault_common::config::PartitionRetentionConfig::default();
        cfg.market_data_hot_days = 35;
        cfg.pressure_archive_enabled = false;
        let hot = crate::partition_archive::hot_window_days(
            crate::top_volume_rank_persistence::SnapshotCadence::OneSecond.table_name(),
            &cfg,
        );
        assert_eq!(
            hot, 35,
            "top_volume tables archive on the market-data window"
        );
        assert_eq!(top_volume_replay_max_age_secs(&cfg), 34 * 86_400);
        cfg.market_data_hot_days = 0;
        // Floored at the archive's 2 days, less one: never below a day.
        assert_eq!(top_volume_replay_max_age_secs(&cfg), 86_400);
        let policy = top_volume_replay_policy(
            Path::new(crate::top_volume_rank_persistence::TOP_VOLUME_SPILL_DIR),
            &cfg,
        );
        assert_eq!(policy.max_file_age_secs, Some(86_400));
        assert_eq!(policy.window, ReplayWindow::None);
    }

    /// Regression (2026-10-13 PR-C review, MEDIUM): a pressure pass cuts
    /// market data to the pressure window, so the age limit follows the
    /// smaller window whenever pressure archival is on; otherwise a file 2 to
    /// 14 days old was replayed into hours the pressure leg had dropped.
    #[test]
    fn test_regression_top_volume_age_limit_follows_the_pressure_window() {
        let mut cfg = tickvault_common::config::PartitionRetentionConfig::default();
        cfg.market_data_hot_days = 15;
        cfg.pressure_hot_days = 2;
        cfg.pressure_archive_enabled = false;
        assert_eq!(top_volume_replay_max_age_secs(&cfg), 14 * 86_400);
        cfg.pressure_archive_enabled = true;
        assert_eq!(
            top_volume_replay_max_age_secs(&cfg),
            86_400,
            "pressure archives top_volume at 2 days, so files older than 1 day are set aside"
        );
        cfg.pressure_hot_days = 5;
        assert_eq!(top_volume_replay_max_age_secs(&cfg), 4 * 86_400);
        cfg.pressure_hot_days = 0;
        assert_eq!(
            top_volume_replay_max_age_secs(&cfg),
            86_400,
            "the pressure window is floored at 2 days, so never below one day"
        );
        // Pressure only ever shortens: a pressure window above the normal one
        // changes nothing.
        cfg.pressure_hot_days = 40;
        assert_eq!(top_volume_replay_max_age_secs(&cfg), 14 * 86_400);
    }

    #[test]
    fn test_spill_file_older_than_is_measured_from_the_hour_start() {
        let day = 86_400;
        let hour = 1_000_u64;
        let start = hour * SPILL_FILE_HOUR_SECS;
        assert!(
            !spill_file_older_than(hour, day, start + day),
            "exactly at the limit: kept"
        );
        assert!(spill_file_older_than(hour, day, start + day + 1));
        assert!(
            !spill_file_older_than(hour, day, start),
            "a current file is never old"
        );
        assert!(
            !spill_file_older_than(u64::MAX, day, u64::MAX),
            "saturates, no panic"
        );
    }

    #[test]
    fn test_set_aside_aged_spill_files_moves_only_old_pending_closed_files() {
        let dir = temp_dir("q4b-aged").join("top_volume");
        std::fs::create_dir_all(&dir).expect("mkdir"); // APPROVED: test
        let now = SpillSinkView::current().now_unix_secs;
        let this_hour = now / SPILL_FILE_HOUR_SECS;
        let old = dir.join(format!("top_volume-dhan-{}.ilp", this_hour - 48));
        let old_empty = dir.join(format!("top_volume-dhan-{}.ilp", this_hour - 47));
        let fresh = dir.join(format!("top_volume-dhan-{this_hour}.ilp"));
        std::fs::write(&old, GOOD_LINE).expect("write"); // APPROVED: test
        std::fs::write(&old_empty, b"").expect("write"); // APPROVED: test
        std::fs::write(&fresh, GOOD_LINE).expect("write"); // APPROVED: test
        let mut view = test_spill_view();
        view.now_unix_secs = now + 600;
        // Written just now: not yet quiet, so even the old-named file stays.
        let mut early = view;
        early.now_unix_secs = now;
        assert_eq!(set_aside_aged_spill_files(&dir, 86_400, &early), 0);
        assert_eq!(set_aside_aged_spill_files(&dir, 86_400, &view), 1);
        assert!(!old.exists(), "the old pending file is set aside");
        assert!(
            dir.join(QUARANTINE_DIR)
                .join(old.file_name().expect("name"))
                .exists()
        );
        assert!(old_empty.exists(), "an empty file has nothing to replay");
        assert!(
            fresh.exists(),
            "a file inside the age limit is replayed, not moved"
        );
        let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
    }

    /// Answers 204 to every POST and records each body.
    async fn spawn_recording_server() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind"); // APPROVED: test
        let addr = listener.local_addr().expect("addr"); // APPROVED: test
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&bodies);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req: Vec<u8> = Vec::new();
                let mut buf = vec![0_u8; 65536];
                let mut body_start = None;
                let mut content_len = 0_usize;
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if body_start.is_none()
                        && let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        body_start = Some(i + 4);
                        let head = String::from_utf8_lossy(&req[..i]).to_ascii_lowercase();
                        content_len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                    if let Some(start) = body_start
                        && req.len() >= start + content_len
                    {
                        break;
                    }
                }
                let start = body_start.unwrap_or(req.len()).min(req.len());
                seen.lock().expect("lock").push(req[start..].to_vec()); // APPROVED: test
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/write"), bodies, handle)
    }

    #[tokio::test]
    async fn test_replayed_top_volume_rows_upsert_onto_themselves() {
        // E1: a re-POST of the same file sends identical bodies, so the
        // DEDUP key `(ts, tf, family, feed, security_id, segment)` upserts
        // the second write onto the first. And E5: a 22:00 IST board line is
        // posted, never filtered.
        let dir = temp_dir("tv-replay-upsert").join("top_volume");
        std::fs::create_dir_all(&dir).expect("mkdir"); // APPROVED: test
        let path = dir.join("top_volume-dhan-1.ilp");
        let mut body = String::new();
        for secs in [9 * 3600 + 30, 22 * 3600] {
            body.push_str(&format!(
                "top_volume_1s,tf=1s,family=stock_option,feed=dhan,segment=NSE_FNO \
                 security_id=7i,total_lots_traded=3i {}\n",
                ist_at(secs)
            ));
        }
        std::fs::write(&path, body.as_bytes()).expect("write"); // APPROVED: test
        let (url, bodies, server) = spawn_recording_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let view = test_spill_view();
        let first = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(first.files_failed, 0);
        forget_resume_offset(&path);
        let second = replay_spill_dir(&dir, &url, &client, &view).await;
        assert_eq!(second.files_failed, 0);
        let posted = bodies.lock().expect("lock").clone(); // APPROVED: test
        assert_eq!(posted.len(), 2, "one POST per round");
        assert_eq!(posted[0], posted[1], "the re-send is byte-identical");
        assert_eq!(
            posted[0],
            body.as_bytes(),
            "nothing filtered, nothing reordered"
        );
        server.abort();
        let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
    }
}
