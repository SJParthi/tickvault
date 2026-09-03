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
//! an aged-out file that still HELD records, and fires `SPILL-RETENTION-01` on
//! the latter with "the replay path has been broken for longer than the
//! retention window". A drained file must therefore end up empty, not absent,
//! or that distinction silently stops working.

use std::path::{Path, PathBuf};

use reqwest::Client;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

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
/// `WatchdogSec=60`, and systemd SIGABRTed a process that was alive and
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
    /// Bytes QuestDB accepted this round.
    /// Files permanently refused and moved aside so the queue keeps moving.
    pub files_quarantined: usize,
    pub bytes_replayed: u64,
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
fn quarantine_spill_file(dir: &Path, path: &Path) -> std::io::Result<std::path::PathBuf> {
    let quarantine = dir.join(QUARANTINE_DIR);
    std::fs::create_dir_all(&quarantine)?;
    let name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unnamed.ilp")); // APPROVED: infallible fallback, no panic on a pathological path
    let target = quarantine.join(name);
    std::fs::rename(path, &target)?;
    Ok(target)
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
pub async fn replay_spill_dir(dir: &Path, url: &str, client: &Client) -> SpillReplayOutcome {
    let mut outcome = SpillReplayOutcome::default();
    // ONE buffer for the whole round, reused across files: a fixed cost per
    // round rather than per file, and the thing that makes the peak resident
    // size independent of how large any spill file has grown.
    let mut buf = vec![0_u8; REPLAY_STREAM_BUFFER_BYTES];
    for path in list_spill_files(dir) {
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
            // Already drained. Leave it alone: truncating it again would
            // refresh its mtime every round and it would never age out.
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

        let mut accepted: u64 = 0;
        let mut failed = false;
        let mut quarantined = false;
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
                // 32 MiB that is not a long line, it is a torn file. The
                // pre-streaming code POSTed it, was 4xx'd, and quarantined it;
                // this reaches the same destination without loading it. Fall
                // through to EOF handling by treating the window as whole.
                None => filled,
            };
            let carried = filled.saturating_sub(cut);
            for range in ilp_chunk_ranges(&buf[..cut], REPLAY_MAX_CHUNK_BYTES) {
                let chunk = buf[range].to_vec();
                let len = chunk.len() as u64;
                match client.post(url).body(chunk).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        accepted = accepted.saturating_add(len);
                    }
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        if is_permanent_refusal(status) {
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

        if quarantined {
            // Moved aside, not drained. Continue to the NEXT file — the whole
            // reason this branch exists is that the backlog behind a poison
            // file is usually intact and just needs to be attempted.
            continue;
        }

        if failed {
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
            Ok(len_now) if len_now != file_len => {
                outcome.bytes_replayed = outcome.bytes_replayed.saturating_add(accepted);
                metrics::counter!("tv_tick_spill_replayed_bytes_total").increment(accepted);
                info!(
                    path = %path.display(),
                    len_before = file_len,
                    len_now,
                    bytes = accepted,
                    "spilled rows were accepted, but the file GREW while it was being drained \
                     — NOT truncating, because that would destroy the bytes appended after the \
                     read. They drain on the next round."
                );
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

        // Every chunk accepted. Truncate rather than delete so the retention
        // sweep can still tell a drained file from an abandoned one.
        match std::fs::File::create(&path) {
            Ok(_) => {
                outcome.files_replayed = outcome.files_replayed.saturating_add(1);
                outcome.bytes_replayed = outcome.bytes_replayed.saturating_add(accepted);
                metrics::counter!("tv_tick_spill_replayed_bytes_total").increment(accepted);
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
async fn run_replay_loop(dir: PathBuf, url: String) {
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
        let outcome = replay_spill_dir(&dir, &url, &client).await;
        if outcome.files_replayed > 0 || outcome.files_failed > 0 {
            info!(
                files_replayed = outcome.files_replayed,
                files_failed = outcome.files_failed,
                bytes_replayed = outcome.bytes_replayed,
                "tick spill drain round complete"
            );
        }
        // Trim quarantine on every round, not only at boot (2026-08-28,
        // round-2 fix). Quarantine is written BY THIS LOOP during the session —
        // a permanently-refused file is set aside here — so a boot-only trim
        // lets a heavy-quarantine day ratchet past the ceiling mid-morning and
        // re-disable the rescue tier until the next boot, which is the exact
        // failure the trim exists to prevent. Cheap: it reads one flat
        // directory and returns immediately when the bytes are inside budget.
        let pruned = crate::tick_persistence::prune_quarantine(
            &dir,
            crate::tick_persistence::tick_spill_max_bytes(),
        );
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
pub fn spawn_supervised_tick_spill_replay(
    dir: PathBuf,
    host: &str,
    http_port: u16,
) -> tokio::task::JoinHandle<()> {
    let url = write_url(host, http_port);
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_replay_loop(dir.clone(), url.clone()));
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
            .find("Ok(len_now) if len_now != file_len")
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

    /// A one-shot local HTTP server that answers `n` requests with `status`.
    ///
    /// # Why a real socket rather than a mock
    ///
    /// The success arm of [`replay_spill_dir`] is the arm that matters: it is
    /// what truncates the file and reports the bytes as recovered. A mock would
    /// prove only that the mock was called. Twenty lines of `TcpListener`
    /// exercise the real `reqwest` round trip, the real status check, and the
    /// real truncate — with no QuestDB and no new dependency.
    fn tiny_server(status: &'static str, n: usize) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 4096];
                // One read is enough: we never inspect the body, and draining it
                // fully would block on a request larger than the buffer.
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(status.as_bytes());
                let _ = sock.flush();
            }
        });
        (format!("http://127.0.0.1:{port}/write"), handle)
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
    async fn replay_spill_dir_truncates_a_file_the_database_accepted() {
        // THE success arm. Everything else in this module is scaffolding around
        // this one outcome: rescued ticks reach the database and the file is
        // left empty rather than deleted, so the retention sweep can still tell
        // a drained file from an abandoned one.
        let dir = temp_dir("ok");
        let path = dir.join("ticks-dhan-1.ilp");
        let payload = b"ticks value=1i 1\nticks value=2i 2\n";
        std::fs::write(&path, payload).expect("write");

        let (url, server) = tiny_server("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n", 1);
        let client = crate::http_client::build_probe_client(5).expect("client");
        let outcome = replay_spill_dir(&dir, &url, &client).await;
        let _ = server.join();

        assert_eq!(outcome.files_replayed, 1);
        assert_eq!(outcome.files_failed, 0);
        assert_eq!(outcome.bytes_replayed, payload.len() as u64);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").len(),
            0,
            "a drained file must be EMPTY, not absent — prune_spill_files \
             distinguishes the two and fires SPILL-RETENTION-01 on the other"
        );
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

        let (url, server) = tiny_server("HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n", 1);
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

        let (url, server) =
            tiny_server("HTTP/1.1 500 Server Error\r\ncontent-length: 0\r\n\r\n", 1);
        let client = crate::http_client::build_probe_client(5).expect("client");
        let outcome = replay_spill_dir(&dir, &url, &client).await;
        let _ = server.join();

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
        let (url, server) = tiny_server("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n", 3);
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
        let handle = spawn_supervised_tick_spill_replay(dir.clone(), "127.0.0.1", 1);
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
}
