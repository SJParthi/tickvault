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

/// Replay passes that may fail to read a segment (open or read I/O error)
/// before it is given up on and archived unread, as before K2.
///
/// B1 review (2026-11-13): the count is now kept ACROSS processes in a small
/// book beside the WAL ([`READ_FAILURE_BOOK_FILE`]). Before, it lived only in
/// memory, and a boot reads a failing segment at most twice (the boot pass,
/// then the first catch-up round, after which the drain stops on it), so the
/// cap could never be reached: a segment that stays unreadable was never
/// given up on, never counted on the paged corrupted counter, and cut every
/// later catch-up drain to one round. Now the third failed read, on whichever
/// boot it falls, gives it up. B1 review 4: the book gains at most one per
/// process, so that read means the third separate boot (two passes in one
/// boot count once); three failed passes in one process still give it up.
/// Honest limit: a book that cannot be read or written falls back to the
/// in-memory count (more retries, never fewer).
pub const WAL_REPLAY_IO_RETRY_CAP: u32 = 3;

/// The file, in the WAL folder, that carries each failing segment's read
/// attempts across restarts (B1 review). One line per segment,
/// `<attempts> <file name>`; written atomically (temporary file, rename),
/// only on a failed read, at the give-up and when a counted segment is read
/// again. Not a `*.wal`, so no listing of segments ever sees it.
pub const READ_FAILURE_BOOK_FILE: &str = "replay_read_failures.tvrf";

/// Counter: replay passes refused because the live WAL folder or `replaying/`
/// could not be listed (K2, 2026-10-26). Local `/metrics` only.
pub const WAL_REPLAY_LISTING_REFUSED_COUNTER: &str = "tv_wal_replay_listing_refused_total";

/// Counter: WAL staging steps (`create_dir_all` of `replaying/`, or one
/// segment's rename into it) that failed (K2, 2026-10-26). A failed stage
/// leaves the segment a `*.wal` that the next pass re-reads, so it is not a
/// loss; it is counted so a stuck staging folder is visible. Local only.
pub const WAL_REPLAY_STAGE_FAILED_COUNTER: &str = "tv_wal_replay_stage_failed_total";

/// Most segments [`SEGMENT_READ_FAILURES`] tracks at once. Past it a new
/// failing segment is KEPT and retried, untracked (B1 review 5; before, it
/// was given up on at once), so the list cannot grow without bound.
const SEGMENT_READ_FAILURES_MAX: usize = 1_024;

/// Read-failure counts per segment FILE NAME (the same segment can be read
/// from the live dir or `replaying/`): `(name, passes in this process, boots
/// counted in the book)`. Touched only on the boot replay's error path and
/// once per successfully read segment, never on the frame path.
static SEGMENT_READ_FAILURES: std::sync::Mutex<Vec<(std::ffi::OsString, u32, u32)>> =
    std::sync::Mutex::new(Vec::new());

/// Records one failed read of `path` and returns its attempts so far: the
/// larger of the failed passes in this process and the boots that failed to
/// read it ([`READ_FAILURE_BOOK_FILE`], B1 review). The book gains at most
/// ONE per process (B1 review 4): a boot reads a failing segment twice in
/// seconds (the boot pass, then the first catch-up round), so counting each
/// pass let one short I/O blip use two of the three attempts and give the
/// whole backlog up on the next boot's first blip. So the cap means three
/// separate boots (or, as before K2's book, three passes in one process).
/// At the cap the segment leaves the book (it is given up on and archived).
///
/// Returns [`WAL_REPLAY_IO_RETRY_CAP`] (give up) when the lock is poisoned.
/// B1 review 5: when the list is FULL a new failing segment is KEPT (returns
/// 1), no longer given up. A pass does not stop at a failed read, so one
/// short volume-wide read error (an impaired disk for one boot) used to give
/// up every segment past the 1,024th on its FIRST failure: archived unread
/// and paged as corrupted although the error cleared by the next boot. The
/// failure direction is "keep more": such a segment is not counted in the
/// book while the list is full, so it is retried on every pass until a slot
/// frees. O(tracked), at most [`SEGMENT_READ_FAILURES_MAX`], plus one small
/// book read and, on the first failure in a process or at the cap, one book
/// write; cold (the replay's error path only). Honest limit: a pass with n
/// failing segments (n at most the list size) rewrites the book up to n times,
/// each at most [`SEGMENT_READ_FAILURES_MAX`] lines, about 48 MiB of writes in
/// the worst pass; not batched.
pub(super) fn note_segment_read_failure(path: &Path) -> u32 {
    let Ok(mut list) = SEGMENT_READ_FAILURES.lock() else {
        return WAL_REPLAY_IO_RETRY_CAP;
    };
    note_segment_read_failure_in(&mut list, path)
}

/// [`note_segment_read_failure`] on an explicit list (the static one in
/// production), so a test can fill a list without touching the shared one.
fn note_segment_read_failure_in(
    list: &mut Vec<(std::ffi::OsString, u32, u32)>,
    path: &Path,
) -> u32 {
    let Some(name) = path.file_name() else {
        return WAL_REPLAY_IO_RETRY_CAP;
    };
    // O(1) EXEMPT: begin — boot replay error path, bounded by SEGMENT_READ_FAILURES_MAX
    let tracked = list.iter().position(|(n, _, _)| n.as_os_str() == name);
    if tracked.is_none() && list.len() >= SEGMENT_READ_FAILURES_MAX {
        // B1 review 5: keep it (see the doc above), never give it up here.
        return 1;
    }
    let book = read_failure_book_path(path);
    let mut entries = book.as_deref().map(read_failure_book).unwrap_or_default();
    let in_book = entries
        .iter()
        .find(|(n, _)| n.as_os_str() == name)
        .map_or(0, |(_, n)| *n);
    let (attempts, book_changes) =
        if let Some((_, passes, boots)) = tracked.and_then(|at| list.get_mut(at)) {
            // Already counted in the book by this process: only the pass
            // count moves.
            *passes = passes.saturating_add(1);
            let attempts = (*passes).max(*boots);
            (attempts, attempts >= WAL_REPLAY_IO_RETRY_CAP)
        } else {
            let boots = in_book.saturating_add(1);
            list.push((name.to_os_string(), 1, boots)); // APPROVED: boot replay error path, bounded list
            (boots, true)
        };
    if book_changes {
        entries.retain(|(n, _)| n.as_os_str() != name);
        if attempts < WAL_REPLAY_IO_RETRY_CAP {
            let boots = list
                .iter()
                .find(|(n, _, _)| n.as_os_str() == name)
                .map_or(attempts, |(_, _, b)| *b);
            entries.push((name.to_os_string(), boots)); // APPROVED: boot replay error path, bounded book
        }
        if let Some(book) = book {
            write_read_failure_book(&book, &mut entries);
        }
    }
    if attempts >= WAL_REPLAY_IO_RETRY_CAP {
        // Given up on and archived: its slot is freed, so given-up segments
        // never fill the list (B1 review 5).
        list.retain(|(n, _, _)| n.as_os_str() != name);
    }
    // O(1) EXEMPT: end
    attempts
}

/// Drops `path`'s failure count after it was read: from this process's list
/// and from the book. One lock and an O(tracked) scan, plus one small book
/// read (a missing book, the normal case, is one failed open), cold.
pub(super) fn forget_segment_read_failures(path: &Path) {
    let Some(name) = path.file_name() else {
        return;
    };
    if let Ok(mut list) = SEGMENT_READ_FAILURES.lock() {
        if !list.is_empty() {
            // O(1) EXEMPT: boot replay, bounded by SEGMENT_READ_FAILURES_MAX
            list.retain(|(n, _, _)| n.as_os_str() != name);
        }
        // Under the same lock, so a note and a forget never race on the book.
        if let Some(book) = read_failure_book_path(path) {
            let mut entries = read_failure_book(&book);
            let before = entries.len();
            // O(1) EXEMPT: boot replay, bounded by SEGMENT_READ_FAILURES_MAX
            entries.retain(|(n, _)| n.as_os_str() != name);
            if entries.len() != before {
                write_read_failure_book(&book, &mut entries);
            }
        }
    }
}

/// The book for a segment read from the WAL folder or its `replaying/`
/// subfolder: always the WAL folder's. `None` for a path with no parent.
fn read_failure_book_path(segment: &Path) -> Option<PathBuf> {
    let parent = segment.parent()?;
    let wal_dir = if parent.file_name().is_some_and(|n| n == REPLAYING_SUBDIR) {
        parent.parent()?
    } else {
        parent
    };
    Some(wal_dir.join(READ_FAILURE_BOOK_FILE))
}

/// The book's entries; empty when it is missing or unreadable (a line it
/// cannot parse is skipped). O(book bytes), cold.
fn read_failure_book(book: &Path) -> Vec<(std::ffi::OsString, u32)> {
    let Ok(text) = std::fs::read_to_string(book) else {
        return Vec::new();
    };
    // O(1) EXEMPT: begin: boot replay error path, at most SEGMENT_READ_FAILURES_MAX lines
    text.lines()
        .filter_map(|line| {
            let (count, name) = line.split_once(' ')?;
            let count = count.parse::<u32>().ok()?;
            (!name.is_empty()).then(|| (std::ffi::OsString::from(name), count))
        })
        .take(SEGMENT_READ_FAILURES_MAX)
        .collect()
    // O(1) EXEMPT: end
}

/// Writes the book (temporary file, then rename), or removes it when nothing
/// is left; keeps at most the newest [`SEGMENT_READ_FAILURES_MAX`] entries. A
/// failed write is logged and otherwise ignored: the in-memory count still
/// holds for this process, so a lost book only means more retries. Cold.
fn write_read_failure_book(book: &Path, entries: &mut Vec<(std::ffi::OsString, u32)>) {
    if entries.is_empty() {
        if let Err(err) = std::fs::remove_file(book)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            debug!(book = ?book, error = %err, "WAL replay read-failure book: remove failed");
        }
        return;
    }
    let excess = entries.len().saturating_sub(SEGMENT_READ_FAILURES_MAX);
    entries.drain(..excess);
    let mut text = String::with_capacity(entries.len() * 48); // APPROVED: boot replay error path, bounded book
    for (name, count) in entries.iter() {
        text.push_str(&count.to_string()); // APPROVED: boot replay error path
        text.push(' ');
        text.push_str(&name.to_string_lossy());
        text.push('\n');
    }
    let tmp = book.with_extension("tvrf.tmp");
    let written = std::fs::write(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, book));
    if let Err(err) = written {
        warn!(
            code = ErrorCode::WsSpill04SegmentUnreadable.code_str(),
            book = ?book,
            error = %err,
            "WAL replay read-failure book could not be written; a failing segment's \
             attempts are counted in this process only (more retries, never fewer)"
        );
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

/// Counts and logs one replay pass refused because a segment folder could not
/// be listed (K2). Nothing is staged or archived; every segment stays where it
/// is for the next pass.
// TEST-EXEMPT: covered by test_regression_k2_unlistable_replaying_folder_refuses_the_pass (asserts the counter moves).
pub(super) fn note_listing_refused(wal_dir: &Path, err: &std::io::Error) {
    metrics::counter!(WAL_REPLAY_LISTING_REFUSED_COUNTER).increment(1);
    error!(
        code = ErrorCode::WsSpill04SegmentUnreadable.code_str(),
        wal_dir = ?wal_dir,
        error = %err,
        "WAL replay REFUSED: a segment folder could not be listed, so this pass \
         cannot know which segments it would leave behind. Nothing is staged or \
         archived; every segment stays where it is for the next pass"
    );
}

/// Test-only: forgets `name`'s in-memory count, as a restart does, leaving
/// the book untouched. Kept below every production fn: the loss-counter
/// guard reads a column-0 `#[cfg(test)]` as the start of test code, so one
/// above the counter emits hid them from it.
#[cfg(test)]
pub(super) fn forget_in_memory_for_test(name: &str) {
    if let Ok(mut list) = SEGMENT_READ_FAILURES.lock() {
        list.retain(|(n, _, _)| n.as_os_str() != name);
    }
}

#[cfg(test)]
mod b1_review5_tests {
    use super::*;

    fn temp_wal_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-b1r5-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// Regression, B1 review 5: with more failing segments than the list
    /// holds (a volume-wide read error during one pass), none is given up on
    /// its FIRST failure. Before, every segment past the 1,024th returned the
    /// cap at once and was archived unread and paged as corrupted, although
    /// the error cleared by the next boot.
    ///
    /// Bite: make the full-list arm return `WAL_REPLAY_IO_RETRY_CAP` and the
    /// first assert fails at segment 1,025.
    #[test]
    fn test_regression_a_full_list_keeps_a_new_failing_segment() {
        let dir = temp_wal_dir("full");
        let mut list = Vec::new();
        for i in 0..(SEGMENT_READ_FAILURES_MAX + 8) {
            let path = dir.join(format!("ws-frames-{i:020}.wal"));
            let attempts = note_segment_read_failure_in(&mut list, &path);
            assert!(
                attempts < WAL_REPLAY_IO_RETRY_CAP,
                "segment {i} given up on its first failed read"
            );
        }
        assert_eq!(
            list.len(),
            SEGMENT_READ_FAILURES_MAX,
            "the list stays bounded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B1 review 5: a segment given up on (at the cap) frees its slot, so
    /// given-up segments cannot fill the list over a long process.
    #[test]
    fn test_a_given_up_segment_frees_its_slot() {
        let dir = temp_wal_dir("slot");
        let mut list = Vec::new();
        let path = dir.join("ws-frames-00000000000000000001.wal");
        let mut last = 0;
        for _ in 0..WAL_REPLAY_IO_RETRY_CAP {
            last = note_segment_read_failure_in(&mut list, &path);
        }
        assert_eq!(last, WAL_REPLAY_IO_RETRY_CAP, "three passes give it up");
        assert!(list.is_empty(), "its slot is freed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
