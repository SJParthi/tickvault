//! Length-framed depth spill tier (plan item 49e, depth array rows step 1).
//!
//! # Why depth leaves the text spill
//!
//! The depth spill used to append a failed flush's ILP payload verbatim to a
//! `.ilp` file, and `tick_spill_replay` reads those files back by cutting at
//! newlines and reading the timestamp after the last space. That works only
//! while the payload is ILP v1 TEXT. The depth array-row switch (plan ITEM
//! 49d) needs ILP v2, whose binary doubles and arrays contain both `\n` and
//! spaces, so a spilled v2 batch would be cut apart on replay. This module
//! gives depth a spill format that never looks inside the payload:
//!
//! ```text
//! record = magic "TVDS" | version u8 | 3 reserved zero bytes
//!        | payload length u32 LE | CRC-32 u32 LE | payload
//! ```
//!
//! The CRC covers bytes 4..12 of the header (version, reserved, length) and
//! the payload, so a flipped length or a torn payload is caught. `version` is
//! the ILP protocol version the payload was built with (1 today; 2 once the
//! array table lands).
//!
//! # What replay does with each record
//!
//! A whole record is POSTed in one body, so a v2 batch is never split. A v1
//! record QuestDB permanently refuses is bisected at line boundaries exactly
//! as the text tier does (PR17), so one torn row costs only itself; a v2
//! record that is refused is kept whole in `quarantine/<file>.rejected-records`
//! (framed, so it can be replayed by hand). Bytes that do not form a valid
//! record (a torn write, a flipped bit) are copied to
//! `quarantine/<file>.corrupt-bytes` and the reader resynchronises on the next
//! record whose magic, length and CRC all check. Nothing is deleted.
//!
//! # No window filter here, on purpose
//!
//! The text tier filters replayed lines through the session window because
//! files written by binaries older than 2026-09-05 were spilled before the
//! writers gated their rows. Every framed record is written by this binary or
//! a later one, whose `DepthWriter::append_row` has already refused every
//! out-of-window row, so there is nothing for a replay filter to catch.
//!
//! # Complexity
//!
//! Cold path only: the writer runs after a flush already failed, the reader
//! on the replay task every five minutes. Append is one lock, two writes per
//! 8 MiB of payload and one CRC pass, O(payload). Replay is O(file bytes);
//! a resync after corruption is O(skipped bytes × candidates), with one CRC
//! over at most [`MAX_RECORD_PAYLOAD_BYTES`] per candidate that passes the
//! cheap header checks.

use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};

use reqwest::Client;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

use crate::tick_spill_replay::{
    LineIsolation, QUARANTINE_DIR, REPLAY_LINES_REJECTED_COUNTER, REPLAY_MAX_CHUNK_BYTES,
    SpillReplayOutcome, describe_send_error, forget_resume_offset, ilp_chunk_ranges,
    is_permanent_refusal, isolate_refused_lines, record_resume_offset, resume_offset_for,
    set_aside_rejected_lines,
};

/// File extension of the framed depth spill. Deliberately not `ilp`, so the
/// text replay (`tick_spill_replay::list_spill_files`) never reads a framed
/// file as newline-delimited text.
pub const DEPTH_FRAMED_SPILL_EXTENSION: &str = "dspl";

/// The four bytes every record starts with.
pub const FRAME_MAGIC: [u8; 4] = *b"TVDS";

/// Header size: magic (4), version (1), reserved (3), length (4), CRC (4).
pub const FRAME_HEADER_BYTES: usize = 16;

/// ILP protocol version 1 (text). Every depth row today.
pub const ILP_VERSION_1: u8 = 1;

/// ILP protocol version 2 (binary doubles and arrays). Written once the
/// depth array table lands (plan ITEM 49d step 2).
pub const ILP_VERSION_2: u8 = 2;

/// Largest payload one record may carry. The depth producer buffer is bounded
/// at 32 MiB (`MAX_DEPTH_PRODUCER_BUFFER_BYTES`) and v1 payloads are cut into
/// records of about [`REPLAY_MAX_CHUNK_BYTES`], so a record past this length
/// is never written; the reader treats a header claiming more as corruption,
/// which also bounds the memory one candidate can make it read.
pub const MAX_RECORD_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Counter of byte regions and records set aside to quarantine by the framed
/// replay, labelled by `reason` (`refused`, `corrupt`, `torn_tail`).
pub const FRAMED_SET_ASIDE_COUNTER: &str = "tv_depth_framed_spill_set_aside_total";

/// Suffix of the quarantine file that keeps whole records QuestDB refused.
pub const REJECTED_RECORDS_SUFFIX: &str = ".rejected-records";

/// Suffix of the quarantine file that keeps bytes that did not form a record.
pub const CORRUPT_BYTES_SUFFIX: &str = ".corrupt-bytes";

/// Serialises appends to framed spill files inside this process. The writer
/// thread, the rescue thread and the drain's inline fallback can spill depth
/// at the same moment into the same hour file; a record is two writes (header
/// and payload), and two of them interleaving would cut both records apart.
/// Held only across the writes, never across the sync.
static FRAMED_APPEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// CRC-32 (IEEE) of the header fields after the magic and the payload.
fn record_crc(header_fields: &[u8], payload: &[u8]) -> u32 {
    crate::ws_frame_spill::crc32_ieee_of(&[header_fields, payload])
}

/// The 16-byte header for `payload`.
///
/// # Errors
/// `InvalidInput` when the payload is longer than
/// [`MAX_RECORD_PAYLOAD_BYTES`] or the version is not 1 or 2.
pub fn encode_header(version: u8, payload: &[u8]) -> std::io::Result<[u8; FRAME_HEADER_BYTES]> {
    if version != ILP_VERSION_1 && version != ILP_VERSION_2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown ILP version {version} for a framed depth spill record"),
        ));
    }
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|l| (*l as usize) <= MAX_RECORD_PAYLOAD_BYTES)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "a {}-byte payload is past the {MAX_RECORD_PAYLOAD_BYTES}-byte framed \
                     record limit",
                    payload.len()
                ),
            )
        })?;
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    header[0..4].copy_from_slice(&FRAME_MAGIC);
    header[4] = version;
    header[8..12].copy_from_slice(&len.to_le_bytes());
    let crc = record_crc(&header[4..12], payload);
    header[12..16].copy_from_slice(&crc.to_le_bytes());
    Ok(header)
}

/// Appends `payload` to `file` as one or more framed records and returns how
/// many records were written.
///
/// A v1 payload is cut at line boundaries into records of about
/// [`REPLAY_MAX_CHUNK_BYTES`], the size replay POSTs; a v2 payload is binary
/// and is written as one record. All records of one call are written under
/// one hold of [`FRAMED_APPEND_LOCK`], so no other spill can land between
/// them. The caller syncs.
///
/// # Errors
/// Any write error, or `InvalidInput` from [`encode_header`]. A failed write
/// can leave a torn record at the end of the file; replay sets it aside.
pub fn append_framed_records(
    file: &mut File,
    payload: &[u8],
    version: u8,
) -> std::io::Result<usize> {
    // O(1) EXEMPT: begin — cold path, runs only after a flush already failed.
    let guard = FRAMED_APPEND_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut records = 0_usize;
    if version == ILP_VERSION_1 {
        for range in ilp_chunk_ranges(payload, REPLAY_MAX_CHUNK_BYTES) {
            let part = &payload[range];
            let header = encode_header(version, part)?;
            file.write_all(&header)?;
            file.write_all(part)?;
            records = records.saturating_add(1);
        }
    } else {
        let header = encode_header(version, payload)?;
        file.write_all(&header)?;
        file.write_all(payload)?;
        records = 1;
    }
    file.flush()?;
    drop(guard);
    Ok(records)
    // O(1) EXEMPT: end
}

/// What [`read_record_at`] found at an offset.
#[derive(Debug, PartialEq, Eq)]
pub enum RecordAt {
    /// A whole record whose CRC checks. The payload is in the caller's
    /// scratch buffer; `next` is the offset just after it.
    Whole { version: u8, next: u64 },
    /// The file ends inside the header or the payload: a write still in
    /// flight on the live file, or a torn tail on a closed one.
    Incomplete,
    /// Bad magic, an unknown version, non-zero reserved bytes, a length past
    /// the limit, or a CRC mismatch.
    Corrupt,
}

/// Reads the record starting at `offset` into `scratch` (payload only).
///
/// # Errors
/// Only I/O errors; every malformed shape is a [`RecordAt`] verdict.
pub fn read_record_at(
    file: &File,
    offset: u64,
    file_len: u64,
    scratch: &mut Vec<u8>,
) -> std::io::Result<RecordAt> {
    let header_end = offset.saturating_add(FRAME_HEADER_BYTES as u64);
    if header_end > file_len {
        return Ok(RecordAt::Incomplete);
    }
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    file.read_exact_at(&mut header, offset)?;
    if header[0..4] != FRAME_MAGIC
        || (header[4] != ILP_VERSION_1 && header[4] != ILP_VERSION_2)
        || header[5..8] != [0, 0, 0]
    {
        return Ok(RecordAt::Corrupt);
    }
    let len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if len > MAX_RECORD_PAYLOAD_BYTES {
        return Ok(RecordAt::Corrupt);
    }
    let next = header_end.saturating_add(len as u64);
    if next > file_len {
        return Ok(RecordAt::Incomplete);
    }
    scratch.clear();
    scratch.resize(len, 0);
    file.read_exact_at(scratch, header_end)?;
    let crc = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    if record_crc(&header[4..12], scratch) != crc {
        return Ok(RecordAt::Corrupt);
    }
    Ok(RecordAt::Whole {
        version: header[4],
        next,
    })
}

/// Bytes scanned per read while looking for the next record after a bad one.
const RESYNC_WINDOW_BYTES: usize = 1024 * 1024;

/// The offset of the first whole, CRC-valid record at or after `from`, or
/// `None` when there is none before `file_len`.
///
/// # Errors
/// Only I/O errors.
pub fn find_next_record(
    file: &File,
    from: u64,
    file_len: u64,
    scratch: &mut Vec<u8>,
) -> std::io::Result<Option<u64>> {
    // O(1) EXEMPT: begin — cold, runs only after a corrupt record.
    let mut window = vec![0_u8; RESYNC_WINDOW_BYTES]; // APPROVED: cold path, after corruption
    let mut start = from;
    while start < file_len {
        let want = usize::try_from(file_len - start)
            .unwrap_or(usize::MAX)
            .min(RESYNC_WINDOW_BYTES);
        file.read_exact_at(&mut window[..want], start)?;
        let mut i = 0_usize;
        while i < want {
            if window[i] == FRAME_MAGIC[0] {
                let candidate = start + i as u64;
                if matches!(
                    read_record_at(file, candidate, file_len, scratch)?,
                    RecordAt::Whole { .. }
                ) {
                    return Ok(Some(candidate));
                }
            }
            i += 1;
        }
        // Overlap by the magic length less one, so a magic split across two
        // windows is still seen.
        let advance = want.saturating_sub(FRAME_MAGIC.len() - 1).max(1);
        start = start.saturating_add(advance as u64);
    }
    Ok(None)
    // O(1) EXEMPT: end
}

/// Lists framed depth spill files, oldest first (the name carries the hour).
#[must_use]
pub fn list_framed_spill_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(DEPTH_FRAMED_SPILL_EXTENSION))
        })
        .collect();
    files.sort();
    files
}

/// Appends `bytes` to `<dir>/quarantine/<file name><suffix>` and syncs it.
fn set_aside(dir: &Path, spill_path: &Path, suffix: &str, bytes: &[u8]) -> std::io::Result<()> {
    let quarantine = dir.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&quarantine)?;
    let name = spill_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("depth-spill");
    let target = quarantine.join(format!("{name}{suffix}"));
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)?;
    out.write_all(bytes)?;
    out.sync_data()
}

/// Copies `[from, to)` of `file` to the corrupt-bytes quarantine file.
fn set_aside_region(
    dir: &Path,
    spill_path: &Path,
    file: &File,
    from: u64,
    to: u64,
) -> std::io::Result<u64> {
    // O(1) EXEMPT: begin — cold, after corruption; bounded by the region.
    let len = usize::try_from(to.saturating_sub(from)).unwrap_or(usize::MAX);
    let mut region = vec![0_u8; len]; // APPROVED: cold path, after corruption
    file.read_exact_at(&mut region, from)?;
    set_aside(dir, spill_path, CORRUPT_BYTES_SUFFIX, &region)?;
    Ok(len as u64)
    // O(1) EXEMPT: end
}

/// How one file's walk ended.
enum Walk {
    /// Every record up to the end of the file is dealt with.
    Done,
    /// The live file ends inside a record (a write may be in flight): stop
    /// here and resume next round.
    WaitForWriter,
    /// QuestDB refused transiently or could not be reached, or a quarantine
    /// write failed: stop the round.
    Failed,
}

/// Replays every framed depth spill file in `dir` into QuestDB.
///
/// Same contract as `tick_spill_replay::replay_spill_dir`: oldest file first;
/// the first transient failure stops the round, keeping the offset of the last
/// record dealt with; the live (last) file and a file that grew while it was
/// read are never truncated, their drained offset is remembered instead; a
/// closed file is truncated (not deleted) once every record is dealt with.
/// Replaying a record twice is harmless: the `market_depth` dedup key carries
/// `depth_kind` and `capture_seq`, so a row upserts onto itself.
pub async fn replay_framed_spill_dir(dir: &Path, url: &str, client: &Client) -> SpillReplayOutcome {
    let mut outcome = SpillReplayOutcome::default();
    for reason in ["refused", "corrupt", "torn_tail"] {
        metrics::counter!(FRAMED_SET_ASIDE_COUNTER, "reason" => reason).increment(0);
    }
    let files = list_framed_spill_files(dir);
    let last_index = files.len().saturating_sub(1);
    let mut scratch: Vec<u8> = Vec::new();
    for (file_index, path) in files.into_iter().enumerate() {
        let writer_may_still_append = file_index == last_index;
        let file_len = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "framed depth spill replay could not read a spill file — leaving it for \
                     the next round"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        };
        if file_len == 0 {
            forget_resume_offset(&path);
            outcome.files_skipped_empty = outcome.files_skipped_empty.saturating_add(1);
            continue;
        }
        let resume_from = resume_offset_for(&path).min(file_len);
        if resume_from == file_len {
            outcome.files_skipped_empty = outcome.files_skipped_empty.saturating_add(1);
            continue;
        }
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "framed depth spill replay could not open a spill file — leaving it for \
                     the next round"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        };
        let mut offset = resume_from;
        let mut sent_bytes: u64 = 0;
        let walk = walk_file(
            dir,
            &path,
            &file,
            file_len,
            writer_may_still_append,
            client,
            url,
            &mut offset,
            &mut sent_bytes,
            &mut outcome,
            &mut scratch,
        )
        .await;
        if offset > resume_from {
            record_resume_offset(&path, offset);
        }
        outcome.bytes_replayed = outcome.bytes_replayed.saturating_add(sent_bytes);
        metrics::counter!("tv_tick_spill_replayed_bytes_total").increment(sent_bytes);
        match walk {
            Walk::Failed => {
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
                return outcome;
            }
            Walk::WaitForWriter => {
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                continue;
            }
            Walk::Done => {}
        }
        // Grown while read: the next round resumes at `offset`.
        match std::fs::metadata(&path).map(|m| m.len()) {
            Ok(len_now) if len_now > file_len => {
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                continue;
            }
            Ok(_) => {}
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "could not re-check a drained framed depth spill file's size — NOT \
                     truncating it; the next round resumes at the drained offset"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                return outcome;
            }
        }
        if writer_may_still_append {
            outcome.files_replayed = outcome.files_replayed.saturating_add(1);
            info!(
                path = %path.display(),
                bytes = sent_bytes,
                resume_from = offset,
                "recovered spilled depth rows into the database. This is the hour the writer \
                 may still append to, so the file is left intact and the next round resumes \
                 at the drained offset"
            );
            continue;
        }
        forget_resume_offset(&path);
        match File::create(&path) {
            Ok(_) => {
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                info!(
                    path = %path.display(),
                    bytes = sent_bytes,
                    "recovered spilled depth rows into the database and emptied the closed \
                     spill file"
                );
            }
            Err(err) => {
                error!(
                    code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                    source = "accepted_not_emptied",
                    path = %path.display(),
                    %err,
                    "spilled depth rows were accepted by the database but the spill file \
                     could not be emptied — the rows are saved; the file will be sent again \
                     next round and the duplicate rows collapse onto themselves"
                );
                outcome.files_failed = outcome.files_failed.saturating_add(1);
                metrics::counter!("tv_tick_spill_replay_failed_total").increment(1);
                return outcome;
            }
        }
    }
    outcome
}

/// Walks one file from `*offset`, POSTing each whole record.
#[allow(clippy::too_many_arguments)] // APPROVED: private helper of one caller; the state is the caller's
async fn walk_file(
    dir: &Path,
    path: &Path,
    file: &File,
    file_len: u64,
    live: bool,
    client: &Client,
    url: &str,
    offset: &mut u64,
    sent_bytes: &mut u64,
    outcome: &mut SpillReplayOutcome,
    scratch: &mut Vec<u8>,
) -> Walk {
    let mut accepted_in_file = *offset > 0;
    while *offset < file_len {
        let verdict = match read_record_at(file, *offset, file_len, scratch) {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    offset = *offset,
                    "framed depth spill replay could not read a record — leaving the file \
                     for the next round"
                );
                return Walk::Failed;
            }
        };
        let (version, next) = match verdict {
            RecordAt::Whole { version, next } => (version, next),
            RecordAt::Incomplete if live => return Walk::WaitForWriter,
            RecordAt::Incomplete | RecordAt::Corrupt => {
                // Look for the next good record. On the live file a corrupt
                // stretch with nothing good after it may still be followed by
                // records the writer has not appended yet, so wait.
                let resume = match find_next_record(file, *offset + 1, file_len, scratch) {
                    Ok(r) => r,
                    Err(err) => {
                        warn!(path = %path.display(), %err, "framed depth spill resync could not read");
                        return Walk::Failed;
                    }
                };
                let (to, reason) = match resume {
                    Some(n) => (n, "corrupt"),
                    None if live => return Walk::WaitForWriter,
                    None => (file_len, "torn_tail"),
                };
                match set_aside_region(dir, path, file, *offset, to) {
                    Ok(bytes) => {
                        metrics::counter!(FRAMED_SET_ASIDE_COUNTER, "reason" => reason)
                            .increment(1);
                        error!(
                            code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                            path = %path.display(),
                            offset = *offset,
                            bytes,
                            reason,
                            "bytes in a framed depth spill file did not form a valid record (a \
                             torn write or a damaged disk block). They are kept in quarantine as \
                             <file>.corrupt-bytes and are NOT in the database; the replay carries \
                             on with the next valid record."
                        );
                        *offset = to;
                        continue;
                    }
                    Err(err) => {
                        warn!(
                            path = %path.display(),
                            %err,
                            "bytes that did not form a record could NOT be written to quarantine \
                             — the file is kept intact and retried next round"
                        );
                        return Walk::Failed;
                    }
                }
            }
        };
        let body = bytes::Bytes::copy_from_slice(scratch);
        match client.post(url).body(body.clone()).send().await {
            Ok(resp) if resp.status().is_success() => {
                *sent_bytes = sent_bytes.saturating_add(body.len() as u64);
                accepted_in_file = true;
                *offset = next;
            }
            Ok(resp) if is_permanent_refusal(resp.status().as_u16()) => {
                let status = resp.status().as_u16();
                match set_aside_refused(
                    dir,
                    path,
                    version,
                    &body,
                    accepted_in_file,
                    client,
                    url,
                    outcome,
                )
                .await
                {
                    Some(true) => {
                        accepted_in_file = true;
                        *offset = next;
                    }
                    Some(false) => {
                        warn!(
                            path = %path.display(),
                            status,
                            "a refused framed depth record could not be set aside — the file \
                             is kept intact and retried next round"
                        );
                        return Walk::Failed;
                    }
                    None => return Walk::Failed,
                }
            }
            Ok(resp) => {
                warn!(
                    path = %path.display(),
                    status = resp.status().as_u16(),
                    "framed depth spill replay was refused by QuestDB — the file is kept intact \
                     and retried next round"
                );
                return Walk::Failed;
            }
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error_kind = %describe_send_error(&err),
                    "framed depth spill replay could not reach QuestDB — the file is kept intact \
                     and retried next round"
                );
                return Walk::Failed;
            }
        }
    }
    Walk::Done
}

/// Deals with a record QuestDB permanently refused. `Some(true)`: set aside
/// (in part or whole) and the walk moves on; `Some(false)`: the quarantine
/// write failed; `None`: QuestDB stopped answering during line isolation.
#[allow(clippy::too_many_arguments)] // APPROVED: private helper of one caller
async fn set_aside_refused(
    dir: &Path,
    path: &Path,
    version: u8,
    body: &bytes::Bytes,
    others_accepted: bool,
    client: &Client,
    url: &str,
    outcome: &mut SpillReplayOutcome,
) -> Option<bool> {
    if version == ILP_VERSION_1 {
        match isolate_refused_lines(client, url, body, others_accepted).await {
            LineIsolation::Isolated(rejected) => {
                return Some(match set_aside_rejected_lines(dir, path, body, &rejected) {
                    Ok(bytes) => {
                        let lines = rejected.len() as u64;
                        metrics::counter!(REPLAY_LINES_REJECTED_COUNTER, "dir" => "depth")
                            .increment(lines);
                        outcome.lines_set_aside = outcome.lines_set_aside.saturating_add(lines);
                        error!(
                            code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                            path = %path.display(),
                            lines,
                            bytes,
                            "QuestDB refused single depth rows in a framed spill record. They \
                             are kept in quarantine as <file>.rejected-lines and are NOT in the \
                             database; every other row of the record was accepted."
                        );
                        true
                    }
                    Err(_) => false,
                });
            }
            LineIsolation::Transient => return None,
            LineIsolation::WholeFile => {}
        }
    }
    let Ok(header) = encode_header(version, body) else {
        return Some(false);
    };
    let mut framed = Vec::with_capacity(FRAME_HEADER_BYTES + body.len()); // APPROVED: cold path, after a permanent refusal
    framed.extend_from_slice(&header);
    framed.extend_from_slice(body);
    Some(
        match set_aside(dir, path, REJECTED_RECORDS_SUFFIX, &framed) {
            Ok(()) => {
                metrics::counter!(FRAMED_SET_ASIDE_COUNTER, "reason" => "refused").increment(1);
                outcome.files_quarantined = outcome.files_quarantined.saturating_add(1);
                error!(
                    code = ErrorCode::TickSpill01FileQuarantined.code_str(),
                    path = %path.display(),
                    bytes = body.len(),
                    version,
                    "QuestDB permanently refused a whole framed depth spill record. It is kept, \
                     framed, in quarantine as <file>.rejected-records and is NOT in the database; \
                     the replay carries on with the next record."
                );
                true
            }
            Err(_) => false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("tv-dspl-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir"); // APPROVED: test
        dir
    }

    fn write_records(path: &Path, payloads: &[(&[u8], u8)]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open"); // APPROVED: test
        for (p, v) in payloads {
            append_framed_records(&mut f, p, *v).expect("append"); // APPROVED: test
        }
    }

    fn read_all_records(path: &Path) -> Vec<(u8, Vec<u8>)> {
        let f = File::open(path).expect("open"); // APPROVED: test
        let len = f.metadata().expect("meta").len(); // APPROVED: test
        let mut out = Vec::new();
        let mut off = 0;
        let mut scratch = Vec::new();
        while off < len {
            match read_record_at(&f, off, len, &mut scratch).expect("read") {
                RecordAt::Whole { version, next } => {
                    out.push((version, scratch.clone()));
                    off = next;
                }
                other => panic!("unexpected {other:?} at {off}"),
            }
        }
        out
    }

    /// An HTTP responder that reads the whole body and records it: 400 for a
    /// body containing `poison`, 503 for `busy`, 204 otherwise.
    async fn spawn_recording_server() -> (
        String,
        Arc<Mutex<Vec<Vec<u8>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
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
                let body = req[body_start.unwrap_or(req.len()).min(req.len())..].to_vec();
                let has = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
                let resp = if has(b"poison") {
                    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if has(b"busy") {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                if let Ok(mut b) = seen.lock() {
                    b.push(body);
                }
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/write"), bodies, handle)
    }

    fn client() -> Client {
        crate::http_client::build_probe_client(5).expect("client") // APPROVED: test
    }

    #[test]
    fn test_framed_record_round_trips_v1_and_v2_payloads() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("depth-dhan-1.dspl");
        // A v2-like payload: binary with newlines and spaces inside.
        let v2: &[u8] = b"market_depth,a=b \x10\n\x00 \n=\x0e\x01price \n";
        write_records(&path, &[(b"market_depth,x=1 y=2i 3\n", 1), (v2, 2)]);
        let records = read_all_records(&path);
        assert_eq!(
            records,
            vec![(1, b"market_depth,x=1 y=2i 3\n".to_vec()), (2, v2.to_vec())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_framed_records_cuts_a_large_v1_payload_into_whole_line_records() {
        let dir = temp_dir("cut");
        let path = dir.join("depth-dhan-1.dspl");
        let mut payload = Vec::new();
        let mut sid = 0_u64;
        while payload.len() <= REPLAY_MAX_CHUNK_BYTES + 1024 {
            sid += 1;
            payload
                .extend_from_slice(format!("market_depth,s=1 security_id={sid}i 1\n").as_bytes());
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        assert_eq!(
            append_framed_records(&mut f, &payload, 1).expect("append"),
            2
        );
        drop(f);
        let records = read_all_records(&path);
        assert_eq!(records.len(), 2);
        assert!(
            records.iter().all(|(_, p)| p.ends_with(b"\n")),
            "records hold whole lines"
        );
        let joined: Vec<u8> = records.into_iter().flat_map(|(_, p)| p).collect();
        assert_eq!(joined, payload, "no byte lost or reordered across records");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_encode_header_refuses_unknown_version_and_oversized_payload() {
        assert!(encode_header(3, b"x").is_err());
        assert!(encode_header(0, b"x").is_err());
        let big = vec![0_u8; MAX_RECORD_PAYLOAD_BYTES + 1];
        assert!(encode_header(2, &big).is_err());
        assert!(encode_header(2, &big[..MAX_RECORD_PAYLOAD_BYTES]).is_ok());
    }

    #[test]
    fn test_read_record_at_reports_torn_header_torn_payload_and_bad_crc() {
        let dir = temp_dir("torn");
        let path = dir.join("depth-dhan-1.dspl");
        write_records(&path, &[(b"abc def\n", 1)]);
        let f = File::open(&path).expect("open");
        let full = f.metadata().expect("meta").len();
        let mut s = Vec::new();
        assert_eq!(
            read_record_at(&f, 0, 10, &mut s).expect("r"),
            RecordAt::Incomplete
        );
        assert_eq!(
            read_record_at(&f, 0, full - 1, &mut s).expect("r"),
            RecordAt::Incomplete
        );
        assert!(matches!(
            read_record_at(&f, 0, full, &mut s).expect("r"),
            RecordAt::Whole { version: 1, .. }
        ));
        drop(f);
        // Flip one payload byte: the CRC must catch it.
        let mut bytes = std::fs::read(&path).expect("read");
        let last = bytes.len() - 2;
        bytes[last] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");
        let f = File::open(&path).expect("open");
        assert_eq!(
            read_record_at(&f, 0, full, &mut s).expect("r"),
            RecordAt::Corrupt
        );
        // Flip a length byte: also caught (CRC covers the length).
        bytes[last] ^= 0x01;
        bytes[8] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");
        let f = File::open(&path).expect("open");
        assert_ne!(
            read_record_at(&f, 0, full, &mut s).expect("r"),
            RecordAt::Whole {
                version: 1,
                next: full
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_replay_posts_each_record_whole_and_truncates_a_closed_file() {
        let dir = temp_dir("replay");
        let closed = dir.join("depth-dhan-1.dspl");
        let live = dir.join("depth-dhan-2.dspl");
        let v2: &[u8] = b"bin \n with \n newlines \x00\x01";
        write_records(&closed, &[(b"market_depth,a=1 b=1i 1\n", 1), (v2, 2)]);
        write_records(&live, &[(b"market_depth,a=2 b=2i 2\n", 1)]);
        let (url, bodies, server) = spawn_recording_server().await;
        let out = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        let bodies = bodies.lock().expect("lock").clone();
        assert_eq!(
            bodies,
            vec![
                b"market_depth,a=1 b=1i 1\n".to_vec(),
                v2.to_vec(),
                b"market_depth,a=2 b=2i 2\n".to_vec()
            ],
            "each record POSTed whole, in file order; the v2 body is not cut at its newlines"
        );
        assert_eq!(out.files_replayed, 2);
        assert_eq!(out.files_failed, 0);
        assert_eq!(
            std::fs::metadata(&closed).expect("m").len(),
            0,
            "closed file emptied"
        );
        assert!(
            std::fs::metadata(&live).expect("m").len() > 0,
            "the live file is never truncated"
        );
        // A second round re-sends nothing from the live file.
        let (url, bodies, server) = spawn_recording_server().await;
        let _ = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        assert!(
            bodies.lock().expect("lock").is_empty(),
            "the drained offset is remembered"
        );
        forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_a_torn_tail_on_the_live_file_waits_for_the_writer() {
        let dir = temp_dir("live-torn");
        let live = dir.join("depth-dhan-1.dspl");
        write_records(&live, &[(b"market_depth,a=1 b=1i 1\n", 1)]);
        // Half a record: a write in flight.
        let header = encode_header(1, b"market_depth,a=2 b=2i 2\n").expect("h");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .expect("open");
        f.write_all(&header).expect("w");
        f.write_all(b"market_depth,a=2").expect("w");
        drop(f);
        let (url, bodies, server) = spawn_recording_server().await;
        let out = replay_framed_spill_dir(&dir, &url, &client()).await;
        assert_eq!(
            bodies.lock().expect("lock").len(),
            1,
            "only the whole record is sent"
        );
        assert_eq!(out.files_failed, 0);
        assert!(
            !dir.join(QUARANTINE_DIR).exists(),
            "nothing set aside on the live file"
        );
        // The writer finishes the record; the next round sends exactly it.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .expect("open");
        f.write_all(b" b=2i 2\n").expect("w");
        drop(f);
        let _ = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        let bodies = bodies.lock().expect("lock").clone();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1], b"market_depth,a=2 b=2i 2\n".to_vec());
        forget_resume_offset(&live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_replay_framed_spill_dir_sets_aside_corrupt_bytes_and_resyncs() {
        let dir = temp_dir("resync");
        let closed = dir.join("depth-dhan-1.dspl");
        write_records(&closed, &[(b"market_depth,a=1 b=1i 1\n", 1)]);
        let first_len = std::fs::metadata(&closed).expect("m").len();
        write_records(&closed, &[(b"market_depth,a=2 b=2i 2\n", 1)]);
        let second_end = std::fs::metadata(&closed).expect("m").len();
        write_records(&closed, &[(b"market_depth,a=3 b=3i 3\n", 1)]);
        // Damage the second record's payload.
        let mut bytes = std::fs::read(&closed).expect("read");
        bytes[(second_end - 3) as usize] ^= 0xFF;
        std::fs::write(&closed, &bytes).expect("write");
        // A later file so `closed` is not the live one.
        write_records(
            &dir.join("depth-dhan-2.dspl"),
            &[(b"market_depth,a=4 b=4i 4\n", 1)],
        );
        let (url, bodies, server) = spawn_recording_server().await;
        let out = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        let bodies = bodies.lock().expect("lock").clone();
        assert_eq!(
            bodies,
            vec![
                b"market_depth,a=1 b=1i 1\n".to_vec(),
                b"market_depth,a=3 b=3i 3\n".to_vec(),
                b"market_depth,a=4 b=4i 4\n".to_vec()
            ]
        );
        assert_eq!(out.files_failed, 0);
        let kept = std::fs::read(
            dir.join(QUARANTINE_DIR)
                .join(format!("depth-dhan-1.dspl{CORRUPT_BYTES_SUFFIX}")),
        )
        .expect("kept");
        assert_eq!(
            kept,
            bytes[first_len as usize..second_end as usize].to_vec(),
            "exactly the damaged record's bytes are kept"
        );
        forget_resume_offset(&dir.join("depth-dhan-2.dspl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_a_torn_tail_on_a_closed_file_is_kept_in_quarantine() {
        let dir = temp_dir("closed-torn");
        let closed = dir.join("depth-dhan-1.dspl");
        write_records(&closed, &[(b"market_depth,a=1 b=1i 1\n", 1)]);
        let header = encode_header(1, b"market_depth,a=2 b=2i 2\n").expect("h");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&closed)
            .expect("open");
        f.write_all(&header).expect("w");
        f.write_all(b"market_depth").expect("w");
        drop(f);
        write_records(
            &dir.join("depth-dhan-2.dspl"),
            &[(b"market_depth,a=3 b=3i 3\n", 1)],
        );
        let (url, _bodies, server) = spawn_recording_server().await;
        let out = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        assert_eq!(out.files_failed, 0);
        let kept = std::fs::read(
            dir.join(QUARANTINE_DIR)
                .join(format!("depth-dhan-1.dspl{CORRUPT_BYTES_SUFFIX}")),
        )
        .expect("kept");
        assert_eq!(kept.len(), FRAME_HEADER_BYTES + b"market_depth".len());
        assert_eq!(std::fs::metadata(&closed).expect("m").len(), 0);
        forget_resume_offset(&dir.join("depth-dhan-2.dspl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_a_refused_v2_record_is_kept_whole_and_a_busy_one_stops_the_round() {
        let dir = temp_dir("refused");
        let closed = dir.join("depth-dhan-1.dspl");
        let v2_poison: &[u8] = b"bin poison \n\x00";
        write_records(
            &closed,
            &[
                (v2_poison, 2),
                (b"market_depth,a=1 b=1i 1\n", 1),
                (b"busy\n", 1),
                (b"market_depth,a=2 b=2i 2\n", 1),
            ],
        );
        write_records(
            &dir.join("depth-dhan-2.dspl"),
            &[(b"market_depth,a=3 b=3i 3\n", 1)],
        );
        let (url, bodies, server) = spawn_recording_server().await;
        let out = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        assert_eq!(out.files_failed, 1, "the 503 stops the round");
        let bodies = bodies.lock().expect("lock").clone();
        assert_eq!(
            bodies.len(),
            3,
            "poison, the good record, then busy; nothing after"
        );
        let rejected = dir
            .join(QUARANTINE_DIR)
            .join(format!("depth-dhan-1.dspl{REJECTED_RECORDS_SUFFIX}"));
        let kept = read_all_records(&rejected);
        assert_eq!(kept, vec![(2, v2_poison.to_vec())], "kept framed and whole");
        assert!(
            std::fs::metadata(&closed).expect("m").len() > 0,
            "not truncated after a failure"
        );
        // Next round resumes at the busy record, not at the start.
        let start = resume_offset_for(&closed);
        assert!(start > 0);
        forget_resume_offset(&closed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_isolate_refused_lines_and_set_aside_rejected_lines_keep_a_refused_v1_line() {
        let dir = temp_dir("v1-line");
        let closed = dir.join("depth-dhan-1.dspl");
        write_records(
            &closed,
            &[(
                b"market_depth,a=1 b=1i 1\nmarket_depth,poison\nmarket_depth,a=2 b=2i 2\n",
                1,
            )],
        );
        write_records(
            &dir.join("depth-dhan-2.dspl"),
            &[(b"market_depth,a=3 b=3i 3\n", 1)],
        );
        let (url, _bodies, server) = spawn_recording_server().await;
        let out = replay_framed_spill_dir(&dir, &url, &client()).await;
        server.abort();
        assert_eq!(out.files_failed, 0);
        assert_eq!(out.lines_set_aside, 1);
        let kept = std::fs::read(dir.join(QUARANTINE_DIR).join(format!(
            "depth-dhan-1.dspl{}",
            crate::tick_spill_replay::REJECTED_LINES_SUFFIX
        )))
        .expect("kept");
        assert_eq!(kept, b"market_depth,poison\n");
        forget_resume_offset(&dir.join("depth-dhan-2.dspl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_concurrent_appends_never_interleave_records() {
        let dir = temp_dir("concurrent");
        let path = dir.join("depth-dhan-1.dspl");
        let mut threads = Vec::new();
        for t in 0..4_u8 {
            let p = path.clone();
            threads.push(std::thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .expect("open");
                let payload = vec![b'a' + t; 64 * 1024];
                for _ in 0..50 {
                    append_framed_records(&mut f, &payload, 2).expect("append");
                }
            }));
        }
        for t in threads {
            t.join().expect("join");
        }
        let records = read_all_records(&path);
        assert_eq!(records.len(), 200);
        assert!(records.iter().all(|(_, p)| p.iter().all(|b| *b == p[0])));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_framed_spill_files_sees_the_framed_extension_and_the_text_replay_does_not() {
        let dir = temp_dir("ext");
        write_records(&dir.join("depth-dhan-1.dspl"), &[(b"x 1\n", 1)]);
        assert!(crate::tick_spill_replay::list_spill_files(&dir).is_empty());
        assert_eq!(list_framed_spill_files(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_next_record_skips_garbage_to_the_next_whole_record() {
        let dir = temp_dir("find-next");
        let path = dir.join("depth-dhan-1.dspl");
        let garbage = b"TVDS not a record at all";
        std::fs::write(&path, garbage).expect("garbage");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        append_framed_records(&mut f, b"market_depth x=1 1\n", ILP_VERSION_1).expect("append");
        let file = File::open(&path).expect("reopen");
        let len = file.metadata().expect("meta").len();
        let mut scratch = Vec::new();
        assert_eq!(
            find_next_record(&file, 0, len, &mut scratch).expect("scan"),
            Some(garbage.len() as u64),
            "the false magic at offset 0 fails its CRC, so the scan lands on the real record"
        );
        let after = garbage.len() as u64 + 1;
        assert_eq!(
            find_next_record(&file, after, len, &mut scratch).expect("scan"),
            None,
            "nothing whole starts after the only record's first byte"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_crc32_ieee_of_matches_the_standard_check_value_across_chunks() {
        // The standard CRC-32/IEEE check value for "123456789".
        let whole = crate::ws_frame_spill::crc32_ieee_of(&[b"123456789"]);
        assert_eq!(whole, 0xCBF4_3926);
        let chunked = crate::ws_frame_spill::crc32_ieee_of(&[b"1234", b"", b"56789"]);
        assert_eq!(chunked, whole, "chunking must not change the checksum");
    }

    #[test]
    fn test_resume_offset_for_record_resume_offset_and_forget_resume_offset_round_trip() {
        let path = temp_dir("resume").join("depth-dhan-9.dspl");
        assert_eq!(resume_offset_for(&path), 0);
        record_resume_offset(&path, 42);
        record_resume_offset(&path, 10);
        assert_eq!(
            resume_offset_for(&path),
            42,
            "the offset never moves backwards"
        );
        forget_resume_offset(&path);
        assert_eq!(resume_offset_for(&path), 0);
    }
}
