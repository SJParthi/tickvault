//! Boot-replay read retry and staging bookkeeping (plan item K2, 2026-10-26),
//! kept out of `ws_frame_spill.rs` so the parent stays under its size pin.
//!
//! A segment whose open or read hits an I/O error is kept for the next pass
//! (up to [`WAL_REPLAY_IO_RETRY_CAP`] attempts in one process) instead of
//! being staged and archived unread; a failed staging step is coded and
//! counted. Every function here runs on the boot or catch-up replay, never on
//! the frame path.

use super::*;

/// Counter: segments a replay pass could not open or read (an I/O error) and
/// KEPT for the next pass instead of archiving them unread (K2, 2026-10-26).
/// Local `/metrics` only; the give-up after [`WAL_REPLAY_IO_RETRY_CAP`] is
/// still counted on `tv_wal_replay_corrupted_segments_total`, which pages.
pub const WAL_REPLAY_READ_RETRIED_COUNTER: &str = "tv_wal_replay_segment_read_retried_total";

/// Replay passes in ONE process that may fail to read a segment (open or read
/// I/O error) before it is given up on and archived unread, as before K2.
///
/// Honest limit: the count lives in memory, so a restart starts it over. A
/// segment that fails once per process is therefore kept and retried on every
/// boot rather than archived; that keeps more on disk, never less.
pub const WAL_REPLAY_IO_RETRY_CAP: u32 = 3;

/// Counter: replay passes refused because the live WAL folder or `replaying/`
/// could not be listed (K2, 2026-10-26). Local `/metrics` only.
pub const WAL_REPLAY_LISTING_REFUSED_COUNTER: &str = "tv_wal_replay_listing_refused_total";

/// Counter: WAL staging steps (`create_dir_all` of `replaying/`, or one
/// segment's rename into it) that failed (K2, 2026-10-26). A failed stage
/// leaves the segment a `*.wal` that the next pass re-reads, so it is not a
/// loss; it is counted so a stuck staging folder is visible. Local only.
pub const WAL_REPLAY_STAGE_FAILED_COUNTER: &str = "tv_wal_replay_stage_failed_total";

/// Most segments [`SEGMENT_READ_FAILURES`] tracks at once. Past it a new
/// failing segment is given up on at once (today's behaviour, counted and
/// paged), so the list cannot grow without bound.
const SEGMENT_READ_FAILURES_MAX: usize = 1_024;

/// Read-failure attempts per segment FILE NAME (the same segment can be read
/// from the live dir or `replaying/`). Touched only on the boot replay's error
/// path and once per successfully read segment, never on the frame path.
static SEGMENT_READ_FAILURES: std::sync::Mutex<Vec<(std::ffi::OsString, u32)>> =
    std::sync::Mutex::new(Vec::new());

/// Records one failed read of `path` and returns the attempts so far,
/// including this one. Returns [`WAL_REPLAY_IO_RETRY_CAP`] (give up) when the
/// lock is poisoned or the list is full. O(tracked), at most
/// [`SEGMENT_READ_FAILURES_MAX`], cold.
pub(super) fn note_segment_read_failure(path: &Path) -> u32 {
    let Some(name) = path.file_name() else {
        return WAL_REPLAY_IO_RETRY_CAP;
    };
    let Ok(mut list) = SEGMENT_READ_FAILURES.lock() else {
        return WAL_REPLAY_IO_RETRY_CAP;
    };
    // O(1) EXEMPT: begin — boot replay error path, bounded by SEGMENT_READ_FAILURES_MAX
    if let Some((_, n)) = list.iter_mut().find(|(n, _)| n.as_os_str() == name) {
        *n = n.saturating_add(1);
        return *n;
    }
    // O(1) EXEMPT: end
    if list.len() >= SEGMENT_READ_FAILURES_MAX {
        return WAL_REPLAY_IO_RETRY_CAP;
    }
    list.push((name.to_os_string(), 1)); // APPROVED: boot replay error path, bounded list
    1
}

/// Drops `path`'s failure count after it was read. One lock and an O(tracked)
/// scan, cold; an empty list (the normal case) costs only the lock.
pub(super) fn forget_segment_read_failures(path: &Path) {
    let Some(name) = path.file_name() else {
        return;
    };
    if let Ok(mut list) = SEGMENT_READ_FAILURES.lock()
        && !list.is_empty()
    {
        // O(1) EXEMPT: boot replay, bounded by SEGMENT_READ_FAILURES_MAX
        list.retain(|(n, _)| n.as_os_str() != name);
    }
}

/// Counts and logs one failed staging step of a replay pass (K2). The segment
/// stays a `*.wal`, so the next pass re-reads it: degraded, not lost.
pub(super) fn note_stage_failure(source: &'static str, path: &Path, err: &std::io::Error) {
    metrics::counter!(WAL_REPLAY_STAGE_FAILED_COUNTER, "step" => source).increment(1);
    // APPROVED: warn, not error — nothing is lost: the segment stays a `*.wal` and the next pass re-reads it; counted on WAL_REPLAY_STAGE_FAILED_COUNTER
    warn!(
        code = ErrorCode::HotPath01SyncFsFailed.code_str(),
        source,
        path = ?path,
        error = %err,
        "WAL replay staging step failed: the segment stays a *.wal in the live folder \
         and is read again by the next pass"
    );
}
