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
    /// Files fully accepted by QuestDB and truncated.
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

/// Whether `dir` is the depth spill directory (as opposed to the tick one).
#[must_use]
pub fn is_depth_spill_dir(dir: &Path) -> bool {
    dir_label(dir) == dir_label(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR))
}

/// The spill ceiling of the directory `dir`: the depth ceiling for the depth
/// spill directory, the tick ceiling otherwise. The quarantine trim takes its
/// budget from the directory it trims (until 2026-10-02 the depth quarantine
/// was trimmed against the tick ceiling). O(1) after each ceiling's first
/// resolution.
#[must_use]
pub fn spill_max_bytes_for_dir(dir: &Path) -> u64 {
    if is_depth_spill_dir(dir) {
        crate::depth_persistence::depth_spill_max_bytes()
    } else {
        crate::tick_persistence::tick_spill_max_bytes()
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
fn resume_offset_for(path: &Path) -> u64 {
    resume_offsets()
        .lock()
        .map(|m| m.get(path).copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Record how far a file has been drained. MONOTONE: never moves backwards, so
/// a short round cannot un-drain bytes a longer one already delivered.
fn record_resume_offset(path: &Path, offset: u64) {
    if let Ok(mut m) = resume_offsets().lock() {
        let slot = m.entry(path.to_path_buf()).or_insert(0);
        *slot = (*slot).max(offset);
    }
}

/// Forget a file we will never read again, so the map cannot grow without
/// bound as hours roll over.
fn forget_resume_offset(path: &Path) {
    if let Ok(mut m) = resume_offsets().lock() {
        m.remove(path);
    }
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
/// accepts, set aside to `quarantine/<file>.rejected-lines` (PR17).
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
enum LineIsolation {
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
async fn isolate_refused_lines(
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
fn set_aside_rejected_lines(
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

pub async fn replay_spill_dir(dir: &Path, url: &str, client: &Client) -> SpillReplayOutcome {
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
    // sorts by name (hour), so the LAST entry is the only one that can still be
    // written to. Everything before it has been closed by the hour rollover.
    // That distinction is what makes the truncate below provably safe: a closed
    // file has no appender, so emptying it cannot destroy anything.
    let files = list_spill_files(dir);
    let last_index = files.len().saturating_sub(1);
    for (file_index, path) in files.into_iter().enumerate() {
        let writer_may_still_append = file_index == last_index;
        // Size from METADATA, never from a materialised read. This is the
        // 2026-09-03 fix: `std::fs::read` here held a 21 GB file in RAM.
        let file_len = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "tick spill replay could not read a spill file — leaving it for the next round"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        };
        if file_len == 0 {
            forget_resume_offset(&path);
            // Already drained. Leave it alone: truncating it again would
            // refresh its mtime every round and it would never age out.
            outcome.files_skipped_empty = outcome.files_skipped_empty.saturating_add(1);
            continue;
        }
        // Start where the last round finished, not at 0. See RESUME_OFFSETS:
        // this is what makes a growing file converge WITHOUT moving bytes or
        // truncating a file the writer still holds open.
        let resume_from = resume_offset_for(&path).min(file_len);
        if resume_from == file_len {
            outcome.files_skipped_empty = outcome.files_skipped_empty.saturating_add(1);
            continue;
        }
        let mut handle = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "tick spill replay could not read a spill file — leaving it for the next round"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        };

        if resume_from > 0 {
            use std::io::Seek as _;
            if let Err(err) = handle.seek(std::io::SeekFrom::Start(resume_from)) {
                warn!(
                    path = %path.display(),
                    %err,
                    resume_from,
                    "tick spill replay could not seek to the resume offset — leaving this file \
                     for the next round rather than re-sending from the start"
                );
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
            // Cut at the LAST newline in the window. Everything before it is
            // whole lines; everything after is carried to the next fill.
            let cut = match buf[..filled].iter().rposition(|b| *b == b'\n') {
                // +1 keeps the newline with the line it terminates, matching
                // `ilp_chunk_ranges`.
                Some(nl) => nl.saturating_add(1),
                // No newline in the FINAL read: a tail torn by a crash
                // mid-append. POST it whole and let QuestDB judge it — which
                // is byte-for-byte what this function did before streaming.
                None if eof => filled,
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
            let carried = filled.saturating_sub(cut);
            for range in ilp_chunk_ranges(&buf[..cut], REPLAY_MAX_CHUNK_BYTES) {
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
                let is_arrival_clock = dir_label(dir)
                    == dir_label(Path::new(crate::depth_persistence::DEPTH_SPILL_DIR));
                let (chunk, refused_lines) = retain_lines_in_open_window(raw, is_arrival_clock);
                refused_bytes =
                    refused_bytes.saturating_add(len.saturating_sub(chunk.len() as u64));
                if refused_lines > 0 {
                    metrics::counter!(
                        "tv_spill_replay_rows_out_of_window_refused_total",
                        "dir" => dir_label(dir)
                    )
                    .increment(refused_lines);
                    warn!(
                        code = ErrorCode::StorageGapTickDedupSegment.code_str(),
                        refused_lines,
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
                                                "QuestDB refused single lines in a spill chunk \
                                                 (a torn or malformed row). They are kept in \
                                                 quarantine as <file>.rejected-lines and are \
                                                 NOT in the database; every other row of the \
                                                 chunk was accepted and the drain carries on \
                                                 with the rest of the file."
                                            );
                                            refused_bytes =
                                                refused_bytes.saturating_add(rejected_bytes);
                                            accepted = accepted.saturating_add(len);
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
                            match quarantine_spill_file(dir, &path) {
                                Ok(moved) => {
                                    metrics::counter!(REPLAY_QUARANTINED_COUNTER).increment(1);
                                    outcome.files_quarantined =
                                        outcome.files_quarantined.saturating_add(1);
                                    error!(
                                        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                                        path = %moved.display(),
                                        status,
                                        bytes = file_len,
                                        "tick spill file PERMANENTLY refused by QuestDB and moved to \
                                         quarantine so the rest of the backlog can drain. The rows \
                                         are still on disk and are NOT in the database. Most of the \
                                         file is usually salvageable — filter to well-formed lines \
                                         and re-POST, which is safe to repeat because the ticks \
                                         dedup key carries capture_seq."
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
            match quarantine_spill_file(dir, &path) {
                Ok(moved) => {
                    metrics::counter!(REPLAY_QUARANTINED_COUNTER).increment(1);
                    outcome.files_quarantined = outcome.files_quarantined.saturating_add(1);
                    error!(
                        code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                        path = %moved.display(),
                        bytes = file_len,
                        window = REPLAY_STREAM_BUFFER_BYTES,
                        "tick spill file contains a line longer than the entire read window, \
                         which no writer here produces — the file is corrupt. Moved to \
                         quarantine with every byte intact rather than POSTing a partial row, \
                         which QuestDB can accept with its own timestamp. Rows already accepted \
                         from this file ARE in the database; the remainder is on disk and is \
                         NOT. Salvage by filtering to well-formed lines and re-POSTing, which \
                         is safe to repeat because the ticks dedup key carries capture_seq."
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
                record_resume_offset(&path, resume_from.saturating_add(accepted));
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
                record_resume_offset(&path, file_len);
                info!(
                    path = %path.display(),
                    len_before = file_len,
                    len_now,
                    resume_from = file_len,
                    bytes = accepted,
                    "spilled rows were accepted; the file GREW while draining, so it is left \
                     BYTE-FOR-BYTE INTACT and the next round resumes at the drained offset. \
                     Nothing is moved, nothing is truncated, nothing is re-sent."
                );
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                continue;
            }
            Ok(_) => {}
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    bytes = accepted,
                    "could not re-check a drained spill file's size — NOT truncating, so no \
                     appended bytes can be destroyed. The rows already accepted are in the \
                     database; the next round re-POSTs them idempotently."
                );
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
        if writer_may_still_append {
            record_resume_offset(&path, file_len);
            outcome.files_replayed = outcome.files_replayed.saturating_add(1);
            outcome.bytes_replayed = outcome
                .bytes_replayed
                .saturating_add(accepted.saturating_sub(refused_bytes));
            metrics::counter!("tv_tick_spill_replayed_bytes_total")
                .increment(accepted.saturating_sub(refused_bytes));
            info!(
                path = %path.display(),
                bytes = accepted,
                resume_from = file_len,
                "recovered spilled ticks into the database. This is the hour the writer is \
                 still appending to, so the file is left BYTE-FOR-BYTE INTACT and the next \
                 round resumes at the drained offset — emptying it here could destroy a \
                 concurrent append."
            );
            continue;
        }

        // A closed file: no writer holds it, so truncating is safe. Truncate
        // rather than delete so the retention sweep can still tell a drained
        // file from an abandoned one.
        forget_resume_offset(&path);
        match std::fs::File::create(&path) {
            Ok(_) => {
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                outcome.bytes_replayed = outcome
                    .bytes_replayed
                    .saturating_add(accepted.saturating_sub(refused_bytes));
                metrics::counter!("tv_tick_spill_replayed_bytes_total")
                    .increment(accepted.saturating_sub(refused_bytes));
                info!(
                    path = %path.display(),
                    bytes = accepted,
                    "recovered spilled ticks into the database — rows a failed flush would \
                     otherwise have lost are now queryable"
                );
            }
            Err(err) => {
                // The rows ARE in QuestDB; only the bookkeeping failed. Say so
                // precisely, because the next round will re-POST them and a
                // reader who assumed loss would be doubly wrong.
                error!(
                    path = %path.display(),
                    %err,
                    bytes = accepted,
                    "spilled ticks were accepted by the database but the spill file could not \
                     be emptied — the rows are saved; the file will be sent again next round \
                     and the duplicate rows collapse onto themselves"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
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
async fn run_replay_loop(dir: PathBuf, url: String, require_upload: bool) {
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
        let round = tokio::task::spawn_blocking(move || {
            let outcome = runtime.block_on(replay_spill_dir(&round_dir, &round_url, &round_client));
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
    let url = write_url(host, http_port);
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_replay_loop(dir.clone(), url.clone(), require_upload));
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
mod tests {
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
        let create_at = production
            .find("std::fs::File::create(&path)")
            .expect("the truncate site must still exist");
        let guard_at = production
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
        let outcome = replay_spill_dir(&dir, "http://127.0.0.1:1/write", &client).await;

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
        let outcome = replay_spill_dir(&dir, "http://127.0.0.1:1/write", &client).await;

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
        let outcome = replay_spill_dir(&missing, "http://127.0.0.1:1/write", &client).await;
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
            .split("pub fn spawn_supervised_tick_spill_replay")
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
        let outcome = replay_spill_dir(&dir, &url, &client).await;
        let _ = server.join();

        assert_eq!(outcome.files_replayed, 2);
        assert_eq!(outcome.files_failed, 0);
        assert_eq!(outcome.bytes_replayed, (payload.len() as u64) * 2);

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
        let outcome = replay_spill_dir(&dir, &url, &client).await;
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
        let outcome = replay_spill_dir(&dir, &url, &client).await;
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
        let outcome = replay_spill_dir(&dir, &url, &client).await;
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
        let out = replay_spill_dir(&dir, &url, &client).await;
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
        let out = replay_spill_dir(&dir, &url, &client).await;
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

        let (url, server) = spawn_selective_server().await;
        let client = crate::http_client::build_probe_client(5).expect("client"); // APPROVED: test
        let out = replay_spill_dir(&dir, &url, &client).await;
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
        let first = replay_spill_dir(&dir, &url, &client).await;
        server.abort();
        assert_eq!(first.lines_set_aside, 1, "round 1 sets the torn line aside");
        assert_eq!(first.files_failed, 1, "and stops at the 503");
        assert!(
            resume_offset_for(&path) > 0,
            "the finished first chunk is remembered"
        );

        let (url, server) = spawn_full_body_server(false).await;
        let second = replay_spill_dir(&dir, &url, &client).await;
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
        let out = replay_spill_dir(&dir, &url, &client).await;
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
}
