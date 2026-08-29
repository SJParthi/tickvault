//! The write-ahead log's confirm step must run AFTER the refold, never before.
//!
//! # The defect this pins
//!
//! `confirm_replayed` moves staged segments out of `replaying/` — the only
//! directory the next boot looks in — and into `archive/`, where a pruner
//! later deletes them by age and byte ceiling. Its own doc says to call it
//! "ONLY after the frames returned by `replay_all` have been durably
//! re-captured into the live pipeline".
//!
//! Boot called it unconditionally, thousands of lines BEFORE the Dhan lane's
//! `refold_wal_frames` — the only code that makes that sentence true. Between
//! those two points the segments were archived while their frames existed only
//! in memory. A crash in that window lost them permanently, and the pruner
//! then deleted the raw bytes on a timer, under a doc-comment asserting they
//! had already been persisted.
//!
//! That inverts the foundation of the zero-tick-loss claim: the WAL's whole
//! purpose is that an unclean stop is recoverable, and the confirm step was
//! declaring recovery before recovery was attempted.
//!
//! # Why a source scan
//!
//! The ordering spans two crates and a spawn boundary — boot stages the batch,
//! hands it to the lane, and the lane folds it much later on its own task.
//! There is no single function to unit-test, and reproducing it would mean
//! booting the whole stack against a live database. What CAN be pinned
//! mechanically is the shape: boot must not confirm when a refold is coming,
//! and the lane must confirm once it has folded.

use tickvault_common::source_scan::{production_region, strip_rust_comments};

fn production(path: &str) -> String {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("wal-confirm guard cannot read {path}: {e}"));
    let stripped = strip_rust_comments(&raw);
    production_region(&stripped).unwrap_or(stripped)
}

/// Boot must NOT confirm on the path where the lane will re-fold.
#[test]
fn boot_does_not_confirm_when_the_lane_will_refold() {
    let src = production("src/main.rs");
    let confirms: Vec<&str> = src
        .lines()
        .filter(|l| l.contains("confirm_replayed("))
        .collect();
    assert_eq!(
        confirms.len(),
        1,
        "boot should carry exactly ONE confirm_replayed call, on the no-refold \
         branch. Found {}: {confirms:?}",
        confirms.len()
    );

    // The surviving call must sit inside a branch guarded on the refold NOT
    // happening. Checked by locating the guard and the call, and asserting the
    // guard comes first — an unguarded confirm is the defect itself.
    let guard = src
        .find("dhan_lane_will_refold && !ws_wal_replay_live_feed.is_empty()")
        .expect(
            "boot must branch on `dhan_lane_will_refold && !ws_wal_replay_live_feed.is_empty()` \
             before confirming — without that guard the confirm archives frames the lane has \
             not folded yet, and a crash in between loses them",
        );
    let call = src
        .find("confirm_replayed(")
        .expect("the no-refold branch must still confirm, or unreadable segments re-stage forever");
    assert!(
        guard < call,
        "the refold guard must precede the confirm in source order — otherwise the \
         confirm is unconditional again"
    );
}

/// The lane must confirm after it folds, or nothing ever clears `replaying/`
/// and every boot re-replays the same segments forever.
#[test]
fn the_lane_confirms_after_refolding() {
    let src = production("src/dhan_feed_stack.rs");
    let refold = src
        .find("refold_wal_frames(&mut ingest,")
        .expect("the lane must still call refold_wal_frames");
    let confirm = src.find("confirm_replayed(").expect(
        "the lane MUST confirm after folding. Without it the segments stay in \
         `replaying/` and every subsequent boot replays them again — bounded by the \
         replay byte budget, but permanently repeated work",
    );
    assert!(
        refold < confirm,
        "confirm_replayed must come AFTER refold_wal_frames in the lane — confirming \
         first is the exact defect this guard exists to prevent"
    );
}

/// The operator-facing flush-failure message must not tell the operator that a
/// restart cannot recover the data. It can, and saying otherwise causes the
/// loss it describes.
#[test]
fn the_flush_failure_message_does_not_tell_the_operator_to_avoid_restarting() {
    let src = production("src/dhan_feed_stack.rs");
    for banned in [
        "do not wait for a restart",
        "there is no re-fold path",
        "boot replay DROPS",
    ] {
        assert!(
            !src.contains(banned),
            "the flush-failure error still says {banned:?}. A re-fold path DOES exist \
             (`refold_wal_frames`), so this steers the operator away from the one action \
             that recovers the window — the message causes the loss it reports."
        );
    }
    // Phrase chosen to sit on ONE source line: the message is a continued
    // string literal, so any phrase spanning a `\` break is split by
    // whitespace and `contains` would never match it. (Learned by writing the
    // first version of this assert against a phrase that did.)
    assert!(
        src.contains("so a restart recovers this window"),
        "the corrected message must state plainly that a restart recovers the window"
    );

    // The sibling message in main.rs is deliberately NOT checked here. It says
    // "there is no re-fold path" and fires ONLY on the branch where the lane
    // will not run — where that is exactly true. Banning the phrase globally
    // would force a correct message to become a vague one.
}

/// The re-folded rows must be FLUSHED before their segments are archived.
///
/// # What this exists to stop
///
/// `confirm_replayed` moves the staged segments into `archive/`, which the
/// next boot does not glob. Its own contract is to run "ONLY after the frames
/// returned by `replay_all` have been durably re-captured into the live
/// pipeline".
///
/// Moving the call next to the refold (2026-08-21) shrank that window from
/// several thousand lines to one -- and left it OPEN, because folding writes
/// into an in-memory ILP buffer and the drain that flushes it is not spawned
/// until well below. "Durably re-captured" still meant "buffered", and the
/// segments were archived on the strength of it. Die before the drain's first
/// flush and those rows never reached the database while their raw bytes were
/// filed where nothing looks again. Silent and permanent.
///
/// Ordering is the whole property, so this asserts POSITION, not presence: a
/// flush that runs after the confirm closes nothing.
#[test]
fn the_refolded_rows_are_flushed_before_their_segments_are_archived() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()));

    // Comment-stripped so the long rationale above the call -- which names
    // both symbols -- cannot satisfy an assertion about the CODE.
    let code: String = src
        .lines()
        .map(|l| l.split_once("//").map_or(l, |(before, _)| before))
        .collect::<Vec<_>>()
        .join("\n");

    let refold_at = code
        .find("refold_wal_frames(&mut ingest")
        .expect("the boot refold call site must exist");
    let tail = &code[refold_at..];

    let flush_at = tail.find("ingest.flush()").expect(
        "the re-folded rows must be flushed after the refold -- without \
                 it `confirm_replayed` archives segments whose rows are still \
                 only in the ILP buffer",
    );
    let confirm_at = tail
        .find("confirm_replayed(")
        .expect("the confirm must follow the refold");

    assert!(
        flush_at < confirm_at,
        "the flush must run BEFORE confirm_replayed. Flushing afterwards \
         closes nothing: the segments are already in `archive/`, where the \
         next boot does not look, by the time the rows reach the database."
    );
}

/// The refold must not fold into a void when the seal writer is absent.
///
/// Boot ordering here holds today only TRANSITIVELY: `run_dhan_feed_stack`
/// parks in the token-manager wait, which cannot complete until an SSM read, a
/// TOTP computation and an HTTPS round trip have finished, by which time
/// `main` is long past `spawn_seal_writer_loop`. That is a race won by a wide
/// margin, not an ordering — and a race won by a margin is exactly the shape
/// that breaks silently when either side moves.
///
/// What makes the silent version unrecoverable is the pairing: a seal produced
/// with no sender installed has no absorption tier to fall into (the ring
/// lives inside `SealWriterRunner`), so it is recorded lost — AND the refold
/// then confirms the segments, archiving the only copy of the frames that
/// produced it. Refusing instead leaves the segments in `replaying/`, so the
/// next boot re-stages them.
#[test]
fn the_refold_refuses_to_fold_when_the_seal_writer_is_not_installed() {
    let source = include_str!("../src/dhan_feed_stack.rs");
    let refold_at = source
        .find("let outcome = refold_wal_frames(")
        .expect("the WAL refold call site must exist");
    let head = &source[..refold_at];
    let check_at = head.rfind("global_seal_sender().is_some()").expect(
        "the refold must check that the seal writer is installed BEFORE it folds — \
             without it, a boot that reaches the refold early loses every recovered \
             candle AND archives the frames that produced them",
    );
    let refuse_at = head.rfind("seal_writer_missing").expect(
        "the not-ready branch must report the frames as unfolded, so the segments stay \
         unconfirmed and the next boot re-stages them",
    );
    assert!(
        check_at < refuse_at,
        "the readiness check must precede the refusal it guards"
    );
    assert!(
        source.contains(
            "report_unfolded_wal_frames(&params.wal_replay_live_feed, \"seal_writer_missing\")"
        ),
        "the refusal must route through report_unfolded_wal_frames — silently skipping \
         the refold would leave the operator with no record that recovery was deferred"
    );
    // The refusal must not also confirm: confirming archives the segments,
    // which is the half that makes the loss permanent.
    let confirm_at = source
        .find("ws_frame_spill::confirm_replayed")
        .expect("the confirm call must exist");
    assert!(
        refuse_at < confirm_at,
        "the refusal branch must sit ABOVE the confirm, so a refused refold cannot \
         archive the segments it declined to read"
    );
}
