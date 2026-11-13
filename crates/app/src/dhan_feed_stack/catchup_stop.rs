//! Why a boot WAL catch-up drain stopped, named honestly (2026-10-20, plan
//! item PR W).
//!
//! The catch-up summary line's `stop_reason` fell through to `"drained"` for
//! every stop it did not name. A drain whose last replay pass came back empty
//! because the pass hit its frame cap, its memory guard or the disk floor, or
//! deferred segments past its byte budget, read as `"drained"` while segments
//! were still waiting on disk; so did a drain that stopped because a batch was
//! never acknowledged, or because the replay could not read. Only a drain that
//! really emptied the backlog (`catchup_drained`) may say `"drained"` now.
//!
//! Pure and O(1): a few branch tests, once per boot, no allocation.
//!
//! The lane's wiring is held by the compiler, not by a source scan: every
//! item here is `pub(super)` and used only by the lane, so removing one of
//! its uses (falling back to a literal `"drained"`, or dropping an early-stop
//! assignment) leaves the item unused, and `dead_code` under the CI clippy
//! run (`-D warnings`) fails the build.

/// The early stop the lane records before it knows a more precise one.
pub(super) const STOP_INCOMPLETE: &str = "incomplete";
/// The replay could not read another batch.
pub(super) const STOP_REPLAY_FAILED: &str = "replay_failed";
/// An empty pass put a segment back unread after an I/O error (B1).
pub(super) const STOP_READ_FAILED: &str = "read_failed";
/// The live writers' tables are not keyed yet (the boot DDL was refused), so
/// a round would only rescue its rows to a spill file and archive the
/// segment; the frames stay in the log instead (B1 review).
pub(super) const STOP_TABLE_NOT_KEYED: &str = "table_not_keyed";
/// A replayed batch was never acknowledged by the sinks.
pub(super) const STOP_ACK_TIMEOUT: &str = "ack_timeout";
/// The partition archiver held the old-hour writers out for longer than the
/// bounded wait (PR AA, owner Quote 51): the segments wait for the next drain.
pub(super) const STOP_ARCHIVE_QUIESCE: &str = "archive_quiesce";

/// The reason for an early stop when the final replay pass returned no
/// frames: `frame_cap`, `memory`, `disk` or `deferred`, in that order of
/// precedence; `None` when that pass found the backlog empty.
#[must_use]
pub(super) const fn empty_pass_stop_reason(
    stopped_for_frame_cap: bool,
    stopped_for_memory: bool,
    stopped_for_disk: bool,
    deferred_segments: u64,
) -> Option<&'static str> {
    if stopped_for_frame_cap {
        Some("frame_cap")
    } else if stopped_for_memory {
        Some("memory")
    } else if stopped_for_disk {
        Some("disk")
    } else if deferred_segments > 0 {
        Some("deferred")
    } else {
        None
    }
}

/// [`empty_pass_stop_reason`] read from a replay batch's own flags. A helper
/// so the lane's call fits on one line of `dhan_feed_stack.rs`, which sits at
/// its size pin. `None` means the empty pass really found the backlog empty,
/// which is the ONLY case the lane may call drained.
///
/// B1 (2026-11-13): a segment whose read failed below the retry cap is put
/// back unread, yet it is not "deferred", so an empty pass with only such a
/// segment left used to read as drained, and the drained branch then cleared
/// the unapplied marks that kept that segment from being archived unread at
/// the next boot. It now stops the drain as `read_failed`, so the
/// not-drained branch marks the leftover range unapplied.
#[must_use]
pub(super) const fn batch_stop_reason(
    batch: &tickvault_storage::ws_frame_spill::WalReplayBatch,
) -> Option<&'static str> {
    match empty_pass_stop_reason(
        batch.stopped_for_frame_cap,
        batch.stopped_for_memory,
        batch.stopped_for_disk,
        batch.deferred_segments,
    ) {
        Some(reason) => Some(reason),
        None if batch.read_failed_segments > 0 => Some(STOP_READ_FAILED),
        None => None,
    }
}

/// The stop a catch-up round must take BEFORE it reads anything, given
/// whether the tables the replay writes are keyed (B1 review, 2026-11-13).
/// With a shut gate every replayed row would be rescued to a local spill
/// file (which cannot drain either, the replay of it waits on the same gate)
/// and its segment confirmed and archived, doubling the disk for frames the
/// log already holds. Pure, O(1).
#[must_use]
pub(super) const fn round_start_stop(tables_keyed: bool) -> Option<&'static str> {
    if tables_keyed {
        None
    } else {
        Some(STOP_TABLE_NOT_KEYED)
    }
}

/// How often, in seconds, a replay waiting for its tables to be keyed looks
/// again.
pub(super) const TABLES_KEYED_POLL_SECS: u64 = 1;

/// Waits until `keyed()` holds, `stop()` holds or `deadline` passes, looking
/// every [`TABLES_KEYED_POLL_SECS`]; returns whether the tables are keyed (B1
/// review 2, 2026-11-13).
///
/// The boot refold and the catch-up drain read the gate ONCE when they
/// started, and a boot whose bounded DDL gave up (QuestDB busy applying its
/// own WAL) has its tables keyed by the background re-run within about 30 s.
/// Reading it once skipped the refold and stopped the drain for the whole
/// process, so the crash-window frames (and their candles) waited for the
/// next boot. Bounded by the caller's own deadline, so the dial is never held
/// longer than the drain's clock. Allocation-free; one gate read and one
/// short sleep per second.
pub(super) async fn wait_for_tables_keyed(
    keyed: impl Fn() -> bool,
    stop: impl Fn() -> bool,
    deadline: tokio::time::Instant,
) -> bool {
    loop {
        if keyed() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if stop() || now >= deadline {
            return false;
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(now)
                .min(std::time::Duration::from_secs(TABLES_KEYED_POLL_SECS)),
        )
        .await;
    }
}

/// Seconds kept free before a window's stop line for a refold that starts
/// only after its gate wait (B1 review 5, 2026-11-13). The wait ends this
/// long before the line (08:57 before the 08:58 pre-open stop), so a refold
/// whose gate opened late in the wait still has a minute before the line.
/// Honest limit: the refold itself (fold, flush, ack of one staged batch) is
/// not bounded by this; a refold longer than the margin still dials late, as
/// a boot at that minute did before B1.
pub(super) const REPLAY_GATE_REFOLD_MARGIN_SECS: u64 = 60;

/// How long, in seconds, the boot refold and the catch-up drain TOGETHER may
/// wait for the replay's tables to be keyed before the sockets dial (B1
/// review 3, 2026-11-13). Both waits share one deadline taken once, before
/// the refold's wait.
///
/// Inside the capture window (09:00 to 15:40 IST, and on a Muhurat day the
/// evening window 18:00 to 19:30 as well, B1 review 5) it is ZERO: the gate
/// is read once and a shut gate defers the replay. The staged frames are safe
/// in the write-ahead log and are re-offered by the next boot, while a tick
/// the sockets miss because they are not dialled yet is lost for good (the
/// vendor does not replay). Outside it, the catch-up budget of that moment
/// ([`super::wal_catchup_budget_secs`]), and before a window never past its
/// stop line less [`REPLAY_GATE_REFOLD_MARGIN_SECS`] (08:57 before 09:00,
/// 17:57 before an 18:00 Muhurat session).
///
/// What this bounds is the WAIT, not the dial (B1 review 5). The dial also
/// waits for what follows the wait: the boot refold (its archive lock, fold,
/// flush and ack) and the catch-up drain. So a refold whose gate opened only
/// after a wait starts only while this still reads above zero
/// ([`refold_may_start_after_wait`]), and the catch-up clock is the budget
/// LESS the seconds the gate wait took ([`catchup_budget_after_gate_wait`]):
/// the wait and the catch-up share one budget, and the dark time before the
/// dial is the refold plus one budget, as it was before B1.
/// O(1), one `OnceLock` load (the boot Muhurat flag the writers read).
#[must_use]
pub(super) fn replay_gate_wait_secs(ist_secs_of_day: u64) -> u64 {
    replay_gate_wait_secs_on(ist_secs_of_day, tickvault_common::muhurat::current())
}

/// [`replay_gate_wait_secs`] with today's Muhurat flag given. Pure, O(1).
#[must_use]
pub(super) const fn replay_gate_wait_secs_on(ist_secs_of_day: u64, muhurat_day: bool) -> u64 {
    use tickvault_common::constants::{
        MUHURAT_PERSIST_END_SECS_OF_DAY_IST, MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
        TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
    };
    let start = TICK_PERSIST_START_SECS_OF_DAY_IST as u64;
    let end = TICK_PERSIST_END_SECS_OF_DAY_IST as u64;
    let muhurat_start = MUHURAT_PERSIST_START_SECS_OF_DAY_IST as u64;
    let muhurat_end = MUHURAT_PERSIST_END_SECS_OF_DAY_IST as u64;
    let in_regular = ist_secs_of_day >= start && ist_secs_of_day < end;
    let in_muhurat =
        muhurat_day && ist_secs_of_day >= muhurat_start && ist_secs_of_day < muhurat_end;
    if in_regular || in_muhurat {
        return 0;
    }
    // The stop line sits as far before a window as 08:58 sits before 09:00.
    let lead = start.saturating_sub(super::WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST);
    let next_window = if ist_secs_of_day < start {
        start
    } else if muhurat_day && ist_secs_of_day < muhurat_start {
        muhurat_start
    } else {
        // No window ahead today: the catch-up budget alone.
        return super::wal_catchup_budget_secs(ist_secs_of_day);
    };
    let cutoff = next_window
        .saturating_sub(lead)
        .saturating_sub(REPLAY_GATE_REFOLD_MARGIN_SECS);
    let until_cutoff = cutoff.saturating_sub(ist_secs_of_day);
    let budget = super::wal_catchup_budget_secs(ist_secs_of_day);
    if until_cutoff < budget {
        until_cutoff
    } else {
        budget
    }
}

/// Seconds a replay spent waiting for its gate: zero when the gate was open
/// at the first look, otherwise `elapsed` rounded UP to a whole second (at
/// least one), so any real wait counts. Pure, O(1).
#[must_use]
pub(super) const fn gate_wait_secs(shut_at_start: bool, elapsed: std::time::Duration) -> u64 {
    if !shut_at_start {
        return 0;
    }
    let whole = elapsed.as_secs();
    let up = if elapsed.subsec_nanos() > 0 {
        whole.saturating_add(1)
    } else {
        whole
    };
    if up == 0 { 1 } else { up }
}

/// Whether the boot refold may start now, after a gate wait of `waited_secs`
/// (B1 review 5). A gate open at the first look (`waited_secs == 0`) folds as
/// before B1. A gate that opened only after a wait folds only while
/// [`replay_gate_wait_secs`] still reads above zero: a gate that opened at
/// 08:57:59 used to start a refold that pushed the dial past 09:00, and the
/// pre-open packets were lost for good. A shut-then-late gate instead defers
/// the frames to the next boot (kept in the log). O(1).
#[must_use]
pub(super) fn refold_may_start_after_wait(ist_secs_of_day: u64, waited_secs: u64) -> bool {
    refold_may_start_after_wait_on(
        ist_secs_of_day,
        waited_secs,
        tickvault_common::muhurat::current(),
    )
}

/// [`refold_may_start_after_wait`] with today's Muhurat flag given. Pure.
#[must_use]
pub(super) const fn refold_may_start_after_wait_on(
    ist_secs_of_day: u64,
    waited_secs: u64,
    muhurat_day: bool,
) -> bool {
    waited_secs == 0 || replay_gate_wait_secs_on(ist_secs_of_day, muhurat_day) > 0
}

/// The catch-up drain's clock after a gate wait of `waited_secs`: its budget
/// less the wait, never below zero (B1 review 5). Before, the drain took a
/// fresh budget after the wait, so a crash restart at 15:45 whose gate opened
/// at second 299 kept the sockets dark for the wait, the refold and another
/// 300 s. Now the wait and the drain share one budget. Pure, O(1).
#[must_use]
pub(super) const fn catchup_budget_after_gate_wait(budget_secs: u64, waited_secs: u64) -> u64 {
    budget_secs.saturating_sub(waited_secs)
}

/// [`wait_for_tables_keyed`] for the boot refold, which holds the replay's
/// archive registration (`intent`, a `ReplayIntent` in production) while it
/// is shut: a held registration makes the partition archiver start no
/// partition, and the archiver is the box's automatic disk relief. So a shut
/// gate RELEASES the registration before the wait (B1 review 3); the caller
/// registers again once the wait returns. An open gate keeps it, so the
/// registration `main.rs` took before the archive loops spawned covers the
/// bring-up unbroken, as before. Generic so a test can watch the release.
pub(super) async fn wait_for_tables_keyed_releasing<T>(
    intent: &mut Option<T>,
    keyed: impl Fn() -> bool,
    stop: impl Fn() -> bool,
    deadline: tokio::time::Instant,
) -> bool {
    if keyed() {
        return true;
    }
    drop(intent.take());
    wait_for_tables_keyed(keyed, stop, deadline).await
}

/// Whether a replay may post now (B1 review 3): every table it writes is
/// keyed in this process, except one recorded un-keyable (QuestDB created it
/// itself; no re-run can key it), which no longer holds the others: its rows
/// take their writer's rescue (the tick writer sets `tick_quotes` and
/// `feed_aux_packets` rows aside to their own spill files; the depth writer
/// rescues to the depth spill), held there until the table is repaired by
/// hand. `ticks` itself always counts: an un-keyable `ticks` holds the
/// replay (see [`replay_cannot_open`]). `feed_aux_packets` is never required
/// (B1 review 2). `gate` is asked one table at a time. Cold, a few loads.
#[must_use]
pub(super) fn replay_ready(
    gate: fn(&[tickvault_storage::ilp_table_gate::GatedTable]) -> bool,
    unkeyable: fn(tickvault_storage::ilp_table_gate::GatedTable) -> bool,
    depth: Option<(
        &'static [tickvault_storage::ilp_table_gate::GatedTable],
        Option<&'static [tickvault_storage::ilp_table_gate::GatedTable]>,
    )>,
) -> bool {
    use tickvault_storage::ilp_table_gate::GatedTable;
    let open = |tables: &[GatedTable]| {
        tables
            .iter()
            .all(|t| gate(std::slice::from_ref(t)) || (*t != GatedTable::Ticks && unkeyable(*t)))
    };
    open(&tickvault_storage::ilp_table_gate::LIVE_TICK_MAIN_TABLES)
        && depth.is_none_or(|(writer, levels)| open(writer) && levels.is_none_or(open))
}

/// Whether waiting for the replay's gate is pointless: `ticks` was recorded
/// un-keyable, so no re-run can open it in this process (B1 review 3). The
/// waits stop at once instead of spending their budget. One Acquire load.
#[must_use]
pub(super) fn replay_cannot_open() -> bool {
    tickvault_storage::ilp_table_gate::table_unkeyable(
        tickvault_storage::ilp_table_gate::GatedTable::Ticks,
    )
}

/// The stop a catch-up round must take AFTER its frames were folded and
/// acknowledged, given how many segments its pass put back unread after an
/// I/O error (B1 review, 2026-11-13). The unread segment sorts first, so
/// every later round would read it again and spend its retry attempts within
/// seconds; at the cap it would be given up on and archived unread. The
/// drain stops instead (not drained), so the leftover range is marked
/// unapplied and a later boot retries the segment. The attempt count is
/// kept across boots (B1 review 2, `READ_FAILURE_BOOK_FILE` beside the WAL),
/// so a segment that stays unreadable reaches the cap on the next boot's
/// first read and is given up on (counted and paged): this stop cuts at most
/// one boot's drain short for it, never every boot. Pure, O(1).
#[must_use]
pub(super) const fn round_end_stop(read_failed_segments: u64) -> Option<&'static str> {
    if read_failed_segments > 0 {
        Some(STOP_READ_FAILED)
    } else {
        None
    }
}

/// What the summary reports after the named stops (shutdown, memory, apply
/// lag, round cap, clock) were ruled out: `"drained"` only when the drain
/// really emptied the backlog, otherwise the early stop the loop recorded.
#[must_use]
pub(super) const fn residual_stop_reason(drained: bool, early_stop: &'static str) -> &'static str {
    if drained { "drained" } else { early_stop }
}

/// Whether a quiesce stop needs a line of its own. The catch-up summary is
/// written only after a completed round or a named stop, so a drain the
/// archive lock stopped in round 0 (the usual case, the first chunk of an
/// evening drain) left only a counter (PR AA finding 16).
#[must_use]
pub(super) fn quiesce_stop_needs_own_line(rounds: u32, early_stop: &str) -> bool {
    rounds == 0 && early_stop == STOP_ARCHIVE_QUIESCE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression, PR AA finding 16: a quiesce stop in round 0 logged no
    /// `stop_reason`. It now gets its own line; a later round's stop is
    /// already named by the summary, and other stops never take this line.
    #[test]
    fn test_regression_round_zero_quiesce_stop_is_logged() {
        assert!(quiesce_stop_needs_own_line(0, STOP_ARCHIVE_QUIESCE));
        assert!(!quiesce_stop_needs_own_line(1, STOP_ARCHIVE_QUIESCE));
        for other in [STOP_INCOMPLETE, STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT] {
            assert!(!quiesce_stop_needs_own_line(0, other));
        }
        let code = lane_code();
        let stop = code
            .find("catchup_early_stop = catchup_stop::STOP_ARCHIVE_QUIESCE;")
            .expect("the quiesce stop");
        let tail = &code[stop..];
        let line = tail
            .find("quiesce_stop_needs_own_line(rounds, catchup_early_stop)")
            .expect("the round-0 line");
        let brk = tail.find("break;").expect("the stop breaks");
        assert!(line < brk, "the line is written before the drain stops");
        assert!(tail[line..brk].contains("stop_reason = catchup_early_stop"));
    }

    #[test]
    fn test_batch_stop_reason_reads_the_batch_flags() {
        use tickvault_storage::ws_frame_spill::WalReplayBatch;
        assert_eq!(batch_stop_reason(&WalReplayBatch::default()), None);
        let disk = WalReplayBatch {
            stopped_for_disk: true,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&disk), Some("disk"));
        let deferred = WalReplayBatch {
            deferred_segments: 2,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&deferred), Some("deferred"));
        let cap = WalReplayBatch {
            stopped_for_frame_cap: true,
            stopped_for_disk: true,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&cap), Some("frame_cap"));
    }

    /// Regression, B1 (2026-11-13): an empty pass that put a segment back
    /// unread after an I/O error is NOT a drained backlog. Before, it read as
    /// drained and the drained branch cleared the unapplied marks guarding
    /// that segment, so the next boot archived it unread.
    #[test]
    fn test_regression_an_unread_segment_never_reads_as_drained() {
        use tickvault_storage::ws_frame_spill::WalReplayBatch;
        let unread = WalReplayBatch {
            read_failed_segments: 1,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&unread), Some(STOP_READ_FAILED));
        let both = WalReplayBatch {
            read_failed_segments: 1,
            stopped_for_disk: true,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&both), Some("disk"));
        // The lane decides "drained" from this one verdict, nothing else.
        let code = lane_code();
        assert!(
            code.contains("catchup_drained = catchup_stop::batch_stop_reason(&batch).is_none();"),
            "the drain must judge drained by batch_stop_reason"
        );
    }

    /// Regression, B1 review (2026-11-13): a round never starts while the
    /// replay's tables are not keyed, and a round whose pass put a segment
    /// back unread ends the drain, both as not drained; the lane takes both
    /// stops at the right place (start of a round, after its ack).
    #[test]
    fn test_regression_gate_and_unread_segment_stop_the_drain_not_drained() {
        assert_eq!(round_start_stop(false), Some(STOP_TABLE_NOT_KEYED));
        assert_eq!(round_start_stop(true), None);
        assert_eq!(round_end_stop(1), Some(STOP_READ_FAILED));
        assert_eq!(round_end_stop(0), None);
        assert_eq!(
            residual_stop_reason(false, STOP_TABLE_NOT_KEYED),
            "table_not_keyed"
        );
        let code = lane_code();
        let start = code
            .find("catchup_stop::round_start_stop(round_keyed)")
            .expect("the round-start gate check");
        let wait = code[..start]
            .rfind("catchup_stop::wait_for_tables_keyed(")
            .expect("B1 review 2: the round waits for the gate first");
        // B1 review 3: on the refold's deadline, so the two waits together
        // never hold the dial past one budget.
        assert!(code[wait..start].contains("replay_gate_deadline.min(catchup_deadline),"));
        assert!(code[wait..start].contains("replay_gate_stop,"));
        let replay = code
            .find("replay_all_with_report_fenced(")
            .expect("the catch-up replay read");
        assert!(start < replay, "checked before the round reads a segment");
        // Twice: after a round that folded nothing, and after a full round.
        assert_eq!(
            code.matches("catchup_stop::round_end_stop(round_read_failed)")
                .count(),
            2
        );
        let end = code
            .rfind("catchup_stop::round_end_stop(round_read_failed)")
            .expect("the round-end unread check");
        let rounds_up = code[..end]
            .rfind("rounds = rounds.saturating_add(1);")
            .expect("the round counter");
        let ack = code[..end]
            .rfind("replay_rows_landed(&mut ingest, \"catchup\"")
            .expect("the round's ack");
        assert!(ack < rounds_up, "only after the round's frames were acked");
        assert!(
            code.contains("} else if !boot_tables_keyed {"),
            "the boot refold folds nothing while the tables are not keyed"
        );
        let boot_wait = code
            .find("let boot_tables_keyed = !seal_writer_ready")
            .expect("B1 review 2: the boot refold waits for the gate");
        let boot_lock = code
            .find("QuiesceWriter::BootRefold")
            .expect("the boot refold lock");
        assert!(boot_wait < boot_lock, "waited for before the archive lock");
        // B1 review 3: ONE deadline, taken before the refold's wait, used by
        // both waits; the refold's wait releases the archive registration
        // while the gate is shut and registers again after it.
        let deadline = code
            .find("let replay_gate_deadline = tokio::time::Instant::now()")
            .expect("the shared deadline");
        assert!(deadline < boot_wait, "taken before the refold's wait");
        assert!(
            code[deadline..boot_wait].contains("catchup_stop::replay_gate_wait_secs("),
            "zero in the capture window"
        );
        assert_eq!(code.matches("replay_gate_deadline = ").count(), 1);
        let boot_call = &code[boot_wait..boot_lock];
        assert!(boot_call.contains("catchup_stop::wait_for_tables_keyed_releasing("));
        assert!(boot_call.contains("&mut old_hour_replay,"));
        assert!(boot_call.contains("replay_gate_deadline,"));
        let rebind = code[boot_wait..]
            .find("let _old_hour_replay = old_hour_replay")
            .map(|i| boot_wait + i)
            .expect("registered again after the wait");
        assert!(rebind < boot_lock);
        assert!(
            !code[..boot_wait].contains("let _old_hour_replay"),
            "nothing registers the replay before the wait releases it"
        );
    }

    /// B1 review 3 (2026-11-13): in the capture window the replay never
    /// waits for its gate (live ticks missed while the sockets are not
    /// dialled are lost for good; the staged frames are safe in the log);
    /// outside it the wait is the catch-up budget of that moment.
    #[test]
    fn test_replay_gate_wait_secs_is_zero_in_the_capture_window() {
        let hm = |h: u64, m: u64| h * 3600 + m * 60;
        for t in [hm(9, 0), hm(11, 20), hm(15, 39) + 59] {
            assert_eq!(replay_gate_wait_secs(t), 0, "{t}");
        }
        for t in [hm(15, 40), hm(20, 0), hm(2, 0), hm(8, 30), hm(8, 50)] {
            assert_eq!(
                replay_gate_wait_secs(t),
                super::super::wal_catchup_budget_secs(t),
                "{t}"
            );
            assert!(replay_gate_wait_secs(t) > 0, "{t}");
        }
    }

    /// Regression, B1 review 4: a pre-open boot's gate wait ends by 08:58, so
    /// the dial is never pushed past 09:00 by the wait. Before, a boot at
    /// 08:59:35 waited up to 20 s (the in-session floor of the catch-up
    /// budget) and the catch-up drain then took a fresh 20 s, so a gate that
    /// opened at second 19 dialled after 09:00:15 and lost the pre-open
    /// packets.
    #[test]
    fn test_regression_a_pre_open_gate_wait_never_runs_past_0858() {
        let hms = |h: u64, m: u64, s: u64| h * 3600 + m * 60 + s;
        assert_eq!(replay_gate_wait_secs(hms(8, 59, 35)), 0);
        // B1 review 5: the wait ends a refold margin before 08:58.
        assert_eq!(replay_gate_wait_secs(hms(8, 56, 50)), 10);
        assert_eq!(replay_gate_wait_secs(hms(8, 57, 50)), 0);
        assert_eq!(replay_gate_wait_secs(hms(8, 58, 0)), 0);
        let stop =
            super::super::WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST - REPLAY_GATE_REFOLD_MARGIN_SECS;
        let start = tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST as u64;
        for t in (start - 3_600)..start {
            let ends = t + replay_gate_wait_secs(t);
            assert!(ends <= stop.max(t), "boot at {t}: the wait ends at {ends}");
        }
    }

    /// Regression, B1 review 5: the refold that follows a gate wait, and the
    /// catch-up drain after it, are bounded too, not only the wait. Before, a
    /// pre-open gate that opened at 08:57:59 started the refold (archive
    /// lock, fold, flush, ack) and then a fresh catch-up budget, so the dial
    /// slipped past 09:00; an evening restart waited 300 s and then drained
    /// another 300 s.
    #[test]
    fn test_regression_refold_and_catchup_after_a_gate_wait_are_bounded() {
        let hms = |h: u64, m: u64, s: u64| h * 3600 + m * 60 + s;
        // A gate that opens at 08:57:59 after a wait never starts the refold.
        assert!(!refold_may_start_after_wait_on(
            hms(8, 57, 59),
            1_000,
            false
        ));
        // An open gate at the first look folds as before B1, at any time.
        assert!(refold_may_start_after_wait_on(hms(8, 57, 59), 0, false));
        assert!(refold_may_start_after_wait_on(hms(10, 0, 0), 0, false));
        // A gate that opens early in the wait still folds.
        assert!(refold_may_start_after_wait_on(hms(8, 40, 0), 30, false));
        // The evening: the wait and the catch-up share one budget.
        let evening = hms(15, 45, 0);
        let budget = super::super::wal_catchup_budget_secs(evening);
        for waited in [0, 1, 120, budget - 1, budget, budget + 50] {
            let rest = catchup_budget_after_gate_wait(budget, waited);
            assert!(
                waited.min(budget) + rest <= budget,
                "waited {waited} + catch-up {rest} exceeds one budget {budget}"
            );
        }
        assert_eq!(catchup_budget_after_gate_wait(300, 299), 1);
        assert_eq!(catchup_budget_after_gate_wait(20, 0), 20);
        // Only a real wait counts.
        use std::time::Duration;
        assert_eq!(gate_wait_secs(false, Duration::from_secs(40)), 0);
        assert_eq!(gate_wait_secs(true, Duration::ZERO), 1);
        assert_eq!(gate_wait_secs(true, Duration::from_millis(1_500)), 2);
        assert_eq!(gate_wait_secs(true, Duration::from_secs(7)), 7);
        // Every pre-open boot: a wait that ends at or before the cutoff and a
        // refold that then may start leave the margin before 08:58.
        let start = tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST as u64;
        let line = super::super::WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST;
        for boot in (start - 3_600)..start {
            let wait = replay_gate_wait_secs_on(boot, false);
            for opened in [1, wait / 2, wait] {
                let at = boot + opened;
                if opened > 0 && refold_may_start_after_wait_on(at, opened, false) {
                    assert!(
                        at + REPLAY_GATE_REFOLD_MARGIN_SECS <= line,
                        "boot {boot}, opened after {opened} s: refold starts at {at}"
                    );
                }
            }
        }
        // The lane wires all three.
        let code = lane_code();
        assert!(code.contains("catchup_stop::refold_may_start_after_wait("));
        assert!(code.contains("catchup_stop::catchup_budget_after_gate_wait("));
        assert!(code.contains("catchup_stop::gate_wait_secs("));
    }

    /// Regression, B1 review 5: on a Muhurat day the evening session is
    /// capture time too. Before, a boot at 18:05 with a shut gate waited the
    /// full 300 s catch-up budget into the session while live Muhurat ticks
    /// were missed for good; a boot before 18:00 now stops waiting by 17:57.
    #[test]
    fn test_regression_muhurat_evening_session_is_capture_time() {
        let hms = |h: u64, m: u64, s: u64| h * 3600 + m * 60 + s;
        assert_eq!(replay_gate_wait_secs_on(hms(18, 5, 0), true), 0);
        assert_eq!(replay_gate_wait_secs_on(hms(19, 29, 59), true), 0);
        assert!(!refold_may_start_after_wait_on(hms(18, 5, 0), 10, true));
        // An ordinary day keeps the evening budget.
        let budget = super::super::wal_catchup_budget_secs(hms(18, 5, 0));
        assert_eq!(replay_gate_wait_secs_on(hms(18, 5, 0), false), budget);
        // Before a Muhurat session the wait ends at 17:57.
        assert_eq!(replay_gate_wait_secs_on(hms(17, 50, 0), true), budget);
        assert_eq!(replay_gate_wait_secs_on(hms(17, 56, 0), true), 60);
        assert_eq!(replay_gate_wait_secs_on(hms(17, 57, 0), true), 0);
        // After the Muhurat window the budget again.
        assert_eq!(
            replay_gate_wait_secs_on(hms(19, 30, 0), true),
            super::super::wal_catchup_budget_secs(hms(19, 30, 0))
        );
    }

    /// Regression, B1 review 3 (2026-11-13): the refold's wait and the
    /// catch-up round's wait share ONE deadline. Before, each took its own
    /// clock, so with a gate that never opens the sockets stayed dark for two
    /// budgets (2 x 20 s in session). With the shared deadline the whole
    /// pre-dial wait is one budget, and the round's wait is instant once the
    /// refold's has run out.
    #[tokio::test(start_paused = true)]
    async fn test_regression_refold_and_round_waits_share_one_deadline() {
        use std::time::Duration;
        let start = tokio::time::Instant::now();
        let budget = replay_gate_wait_secs(20 * 3600);
        let shared = start + Duration::from_secs(budget);
        let mut intent = Some(());
        assert!(!wait_for_tables_keyed_releasing(&mut intent, || false, || false, shared).await);
        // The catch-up clock is taken later and is longer (an evening drain).
        let catchup_deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        assert!(!wait_for_tables_keyed(|| false, || false, shared.min(catchup_deadline)).await);
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(budget),
            "one budget, not two"
        );
        // In the capture window: no wait at all.
        let start = tokio::time::Instant::now();
        let in_session = start + Duration::from_secs(replay_gate_wait_secs(11 * 3600));
        assert!(!wait_for_tables_keyed(|| false, || false, in_session).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// Regression, B1 review 3 (2026-11-13): while the gate is shut the
    /// refold's wait does NOT hold the replay's archive registration (a held
    /// one makes the archiver, the box's disk relief, start no partition);
    /// with the gate open it keeps it.
    #[tokio::test(start_paused = true)]
    async fn test_regression_a_shut_gate_wait_releases_the_archive_registration() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        struct Registration(Arc<AtomicBool>);
        impl Drop for Registration {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&released);
        let watcher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            seen.load(Ordering::SeqCst)
        });
        let mut intent = Some(Registration(Arc::clone(&released)));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        assert!(!wait_for_tables_keyed_releasing(&mut intent, || false, || false, deadline).await);
        assert!(intent.is_none());
        assert!(
            watcher.await.expect("watcher"),
            "released while the wait was still running"
        );
        let kept = Arc::new(AtomicBool::new(false));
        let mut intent = Some(Registration(Arc::clone(&kept)));
        assert!(wait_for_tables_keyed_releasing(&mut intent, || true, || false, deadline).await);
        assert!(
            intent.is_some() && !kept.load(Ordering::SeqCst),
            "an open gate keeps it"
        );
    }

    /// Regression, B1 review 3 (2026-11-13): a table recorded un-keyable no
    /// longer holds the replay (its rows take their writer's rescue), except
    /// `ticks`; a table merely not keyed YET still does.
    #[test]
    fn test_regression_an_unkeyable_table_other_than_ticks_does_not_hold_the_replay() {
        use tickvault_storage::ilp_table_gate::GatedTable;
        const DEPTH: &[GatedTable] = &[GatedTable::MarketDepth];
        let depth = Some((DEPTH, None));
        let all_but_depth: fn(&[GatedTable]) -> bool = |t| t != [GatedTable::MarketDepth];
        let none: fn(GatedTable) -> bool = |_| false;
        let depth_unkeyable: fn(GatedTable) -> bool = |t| t == GatedTable::MarketDepth;
        assert!(
            !replay_ready(all_but_depth, none, depth),
            "not keyed yet: wait"
        );
        assert!(
            replay_ready(all_but_depth, depth_unkeyable, depth),
            "un-keyable depth does not hold the ticks"
        );
        let all_but_quotes: fn(&[GatedTable]) -> bool = |t| t != [GatedTable::TickQuotes];
        let quotes_unkeyable: fn(GatedTable) -> bool = |t| t == GatedTable::TickQuotes;
        assert!(!replay_ready(all_but_quotes, none, None));
        assert!(replay_ready(all_but_quotes, quotes_unkeyable, None));
        let all_but_ticks: fn(&[GatedTable]) -> bool = |t| t != [GatedTable::Ticks];
        let every: fn(GatedTable) -> bool = |_| true;
        assert!(
            !replay_ready(all_but_ticks, every, depth),
            "ticks always counts"
        );
        let open: fn(&[GatedTable]) -> bool = |_| true;
        assert!(replay_ready(open, none, depth));
        assert!(
            !replay_cannot_open(),
            "nothing is un-keyable in this process"
        );
    }

    /// Regression, B1 review 2 (2026-11-13): the refold and the catch-up read
    /// the gate ONCE and gave up for the whole process; the background DDL
    /// re-run keys the tables about 30 s later. The wait now sees a gate that
    /// opens part way through its deadline, and still gives up at the
    /// deadline or on a stop.
    #[tokio::test(start_paused = true)]
    async fn test_regression_wait_for_tables_keyed_sees_a_gate_that_opens_later() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let open = Arc::new(AtomicBool::new(false));
        let opener = Arc::clone(&open);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            opener.store(true, Ordering::SeqCst);
        });
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(300);
        let gate = Arc::clone(&open);
        assert!(
            wait_for_tables_keyed(move || gate.load(Ordering::SeqCst), || false, deadline).await,
            "a gate that opens at 30 s within a 300 s deadline is waited for"
        );
        let waited = start.elapsed();
        assert!(waited >= Duration::from_secs(30) && waited <= Duration::from_secs(31));
        // Never opens: false at the deadline, not before.
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(20);
        assert!(!wait_for_tables_keyed(|| false, || false, deadline).await);
        assert_eq!(start.elapsed(), Duration::from_secs(20));
        // A stop ends the wait at once.
        let start = tokio::time::Instant::now();
        assert!(!wait_for_tables_keyed(|| false, || true, start + Duration::from_secs(300)).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
        // Already keyed: no wait at all.
        assert!(wait_for_tables_keyed(|| true, || true, start).await);
    }

    #[test]
    fn test_empty_pass_stop_reason_names_every_early_stop() {
        assert_eq!(
            empty_pass_stop_reason(true, false, false, 0),
            Some("frame_cap")
        );
        assert_eq!(
            empty_pass_stop_reason(false, true, false, 0),
            Some("memory")
        );
        assert_eq!(empty_pass_stop_reason(false, false, true, 0), Some("disk"));
        assert_eq!(
            empty_pass_stop_reason(false, false, false, 3),
            Some("deferred")
        );
        assert_eq!(empty_pass_stop_reason(false, false, false, 0), None);
        // Precedence when several hold at once: the cap first.
        assert_eq!(
            empty_pass_stop_reason(true, true, true, 9),
            Some("frame_cap")
        );
    }

    /// PR AA step 1: a drain the archive quiesce lock stopped reports
    /// `archive_quiesce`, never `drained`.
    #[test]
    fn test_catchup_stops_with_archive_quiesce_reason() {
        assert_eq!(
            residual_stop_reason(false, STOP_ARCHIVE_QUIESCE),
            "archive_quiesce"
        );
        assert_eq!(residual_stop_reason(true, STOP_ARCHIVE_QUIESCE), "drained");
    }

    /// The lane's code, with its comments removed.
    fn lane_code() -> String {
        include_str!("../dhan_feed_stack.rs")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// PR AA step 1: each catch-up chunk takes the archive quiesce lock
    /// BEFORE its refold, and a miss stops the drain with `archive_quiesce`
    /// before the segment settles.
    #[test]
    fn test_catchup_chunk_takes_the_quiesce_lock_before_its_refold() {
        let code = lane_code();
        let chunk = code
            .find("if !chunk.is_empty() {")
            .expect("the catch-up chunk loop must exist");
        let body = &code[chunk..];
        let lock = body
            .find("aq::QuiesceWriter::CatchUp")
            .expect("each chunk must take the quiesce lock");
        let refold = body
            .find("refold_wal_frames(&mut ingest, chunk")
            .expect("the chunk refold must exist");
        let ack = body
            .find("replay_rows_landed(&mut ingest, \"catchup\"")
            .expect("the chunk ack must exist");
        let settle = body
            .find("settle_confirm_segments(")
            .expect("the segment settle must exist");
        assert!(lock < refold && refold < ack && ack < settle);
        assert!(
            body[lock..refold].contains("round_quiesce_held = true;"),
            "a miss must record the stop before any refold"
        );
        assert!(
            code.contains("catchup_early_stop = catchup_stop::STOP_ARCHIVE_QUIESCE;"),
            "a quiesce-held round must name its stop"
        );
        // PR AA review: the chunk waits to the drain's own deadline (not a
        // fixed 60 s), and the replay is registered before the boot refold,
        // so the archiver starts no new partition while it runs.
        assert!(
            body[..lock].contains("aq::archive_quiesce_for_replay_until(")
                && body[lock..refold].contains("catchup_deadline,"),
            "each chunk waits until the drain's deadline"
        );
        let intent = code
            .find("unwrap_or_else(tickvault_storage::archive_quiesce::intend_old_hour_replay)")
            .expect("the replay must be registered");
        let boot_lock = code
            .find("QuiesceWriter::BootRefold")
            .expect("the boot refold lock");
        assert!(
            intent < boot_lock,
            "registered before the boot refold waits"
        );
    }

    /// Regression, PR AA finding 6: the replay used to be registered only
    /// inside the lane, after the token wait, the seeding and the seal-writer
    /// wait, while both archive loops were already running; on an evening
    /// restart the archiver took the lock first and the drain could miss
    /// its deadline. `main.rs` now registers it BEFORE either archive loop
    /// spawns and hands it to the lane, which reuses it at the refold.
    #[test]
    fn test_regression_boot_replay_is_registered_before_the_archive_loops_spawn() {
        let main_code: String = include_str!("../main.rs")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let intent = main_code
            .find("dhan_feed_stack::boot_replay_intent(&ws_wal_replay_live_feed)")
            .expect("main.rs must register the boot replay");
        for archive_loop in [
            "spawn_supervised_disk_pressure_loop(",
            "spawn_supervised_daily_archive_loop(",
        ] {
            let spawn = main_code
                .find(archive_loop)
                .expect("the archive loop spawn must exist");
            assert!(
                intent < spawn,
                "the replay must be registered before {archive_loop}"
            );
        }
        let handed = main_code
            .find("boot_replay_intent,")
            .expect("main.rs must hand the registration to the lane");
        let lane_spawn = main_code
            .find("dhan_feed_stack::spawn_dhan_feed_stack(")
            .expect("the lane spawn must exist");
        assert!(intent < handed && handed > lane_spawn);
        // The lane reuses it at the refold, and keeps it through the dial
        // wait only up to the authentication grace.
        let lane = lane_code();
        assert!(lane.contains("let mut old_hour_replay = params.boot_replay_intent.take();"));
        let dial = lane
            .find("wait_for_dial_preconditions(&params.instance_lock_held")
            .expect("the dial wait");
        assert!(
            lane[dial..].contains("if params.boot_replay_intent.is_some() =>"),
            "the dial wait releases the registration at the grace"
        );
    }

    /// PR AA finding 6: with nothing staged there is no registration, so the
    /// archiver is never held back on a clean boot. (That a registration
    /// holds the archiver out is pinned in `archive_quiesce`'s own tests.)
    #[test]
    fn test_boot_replay_intent_only_for_staged_frames() {
        let none: [u8; 0] = [];
        assert!(super::super::boot_replay_intent(&none).is_none());
        let intent = super::super::boot_replay_intent(&[1u8]);
        assert!(intent.is_some(), "staged frames register the replay");
        drop(intent);
    }

    /// PR AA step 1: the boot refold takes the lock (try-only in session)
    /// before it folds, folds nothing on a miss, and lets go right after its
    /// flush and ack, before the catch-up drain.
    #[test]
    fn test_boot_refold_takes_the_quiesce_lock_and_releases_it_before_catchup() {
        let code = lane_code();
        let lock = code
            .find("QuiesceWriter::BootRefold")
            .expect("the boot refold must take the quiesce lock");
        let miss = code
            .find("} else if boot_quiesce.is_none() {")
            .expect("a miss must fold nothing");
        let refold = code
            .find("refold_wal_frames(&mut ingest, boot_frames")
            .expect("the boot refold must exist");
        let ack = code
            .find("replay_rows_landed(&mut ingest, \"boot_refold\"")
            .expect("the boot ack must exist");
        let release = code
            .find("drop(boot_quiesce);")
            .expect("the lock must be released explicitly");
        let catchup = code
            .find("let catchup_budget_secs")
            .expect("the catch-up drain must exist");
        assert!(lock < miss && miss < refold && refold < ack && ack < release);
        assert!(
            release < catchup,
            "never held into the catch-up or the live lane"
        );
    }

    /// Regression: 2026-10-20 — a catch-up whose last pass stopped at its
    /// frame cap reported `stop_reason = "drained"` while segments waited.
    #[test]
    fn test_catchup_stop_reason_frame_cap_is_not_drained() {
        let early = empty_pass_stop_reason(true, false, false, 0).unwrap_or(STOP_INCOMPLETE);
        assert_eq!(residual_stop_reason(false, early), "frame_cap");
        assert_ne!(residual_stop_reason(false, early), "drained");
        assert_eq!(residual_stop_reason(true, STOP_INCOMPLETE), "drained");
        assert_eq!(residual_stop_reason(false, STOP_ACK_TIMEOUT), "ack_timeout");
        assert_eq!(
            residual_stop_reason(false, STOP_REPLAY_FAILED),
            "replay_failed"
        );
    }

    /// Every early stop the lane can record, and every empty-pass verdict,
    /// reads as itself unless the drain really emptied the backlog; none of
    /// them reads `"drained"` on its own. Exhaustive over the four inputs
    /// that matter (three flags and whether segments were deferred).
    #[test]
    fn test_residual_stop_reason_is_drained_only_when_the_backlog_emptied() {
        let early_stops = [
            STOP_INCOMPLETE,
            STOP_REPLAY_FAILED,
            STOP_ACK_TIMEOUT,
            STOP_ARCHIVE_QUIESCE,
        ];
        for &early in &early_stops {
            assert_eq!(residual_stop_reason(true, early), "drained");
            assert_eq!(residual_stop_reason(false, early), early);
            assert_ne!(residual_stop_reason(false, early), "drained");
        }
        for bits in 0_u8..16 {
            let (cap, mem, disk) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            let deferred = u64::from(bits & 8 != 0);
            for &prior in &early_stops {
                let early = empty_pass_stop_reason(cap, mem, disk, deferred).unwrap_or(prior);
                let reported = residual_stop_reason(false, early);
                assert_ne!(reported, "drained", "bits {bits:04b}, prior {prior}");
                if cap || mem || disk || deferred > 0 {
                    assert_ne!(reported, prior, "a named stop wins, bits {bits:04b}");
                } else {
                    assert_eq!(reported, prior, "an empty backlog keeps the prior stop");
                }
            }
        }
        // The three names are distinct and none is the drained name.
        assert_ne!(STOP_INCOMPLETE, STOP_REPLAY_FAILED);
        assert_ne!(STOP_INCOMPLETE, STOP_ACK_TIMEOUT);
        assert_ne!(STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT);
        for other in [STOP_INCOMPLETE, STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT] {
            assert_ne!(STOP_ARCHIVE_QUIESCE, other);
        }
        assert!(early_stops.iter().all(|s| *s != "drained"));
    }
}
