//! The WAL applied-watermark (2026-09-05) is only worth anything if PRODUCTION
//! is wired to it. Every claim below is a source-scan on the production
//! region, so a refactor that quietly re-routes a boot path around a fence
//! fails the build here rather than on the next full-disk morning.
//!
//! What this pins, and the incident behind each row:
//!
//! | Pin | Why |
//! |---|---|
//! | STAGE-C and the catch-up call the FENCED replay forms | the unfenced `replay_all` / `replay_all_with_report` exist for fixtures; production on 2026-09-03 replayed 25–75 GB per restart into a volume that then hit 20 KB free |
//! | both lane confirms wait for the writer-thread ACK | `flush()` returns rows HANDED OFF, and `confirm_replayed` archived on the strength of it |
//! | every `RingFull` shed marks the frame unapplied | a shed frame is in the WAL and nowhere else; skipping its segment would be silent loss |
//! | the segment listing excludes the writer's open segment | a catch-up round could stage — and the confirm archive — the file the writer was still appending to |
//! | both sinks advance the watermark on ack AND on durable rescue | a rescued row is re-ingestable; a watermark that ignored rescues would replay every rescued batch forever |
//! | the refusal message no longer claims the segments were archived | it was false since 2026-08-28 and steered the operator away from the restart that recovers the window |

use tickvault_common::source_scan::{production_region, strip_rust_comments};

fn production(path: &str) -> String {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("watermark wiring guard cannot read {path}: {e}"));
    let stripped = strip_rust_comments(&raw);
    production_region(&stripped).unwrap_or(stripped)
}

#[test]
fn stage_c_boot_replay_is_the_fenced_form() {
    let src = production("src/main.rs");
    assert!(
        src.contains("ws_frame_spill::replay_all_fenced("),
        "STAGE-C must call replay_all_fenced — the disk floor and the per-boot frame cap live there"
    );
    assert!(
        !src.contains("ws_frame_spill::replay_all("),
        "the unfenced replay_all is for fixtures; production must not call it"
    );
}

#[test]
fn the_catchup_drain_is_the_fenced_form() {
    let src = production("src/dhan_feed_stack.rs");
    assert!(
        src.contains("ws_frame_spill::replay_all_with_report_fenced("),
        "every catch-up round must go through replay_all_with_report_fenced"
    );
    assert!(
        !src.contains("ws_frame_spill::replay_all_with_report("),
        "the unfenced replay_all_with_report is for fixtures; production must not call it"
    );
}

#[test]
fn both_lane_confirms_wait_for_the_writer_ack() {
    let src = production("src/dhan_feed_stack.rs");
    for stage in ["\"boot_refold\"", "\"catchup\""] {
        let wait = src
            .find(&format!(
                "replay_rows_landed(&mut ingest, {stage}, unlanded_before)"
            ))
            .unwrap_or_else(|| panic!("the {stage} confirm must wait on replay_rows_landed"));
        let confirm = src[wait..]
            .find("confirm_replayed(")
            .expect("a confirm must follow the ack wait");
        // The confirm is inside the `if` the wait guards: no more than a few
        // lines away, and never before it.
        assert!(
            confirm < 400,
            "the {stage} confirm must sit immediately inside the ack-wait guard, not {confirm} bytes later"
        );
    }
    assert!(
        src.contains("if !replay_rows_landed(&mut ingest, \"catchup\", unlanded_before) {"),
        "a catch-up ack timeout must END the drain rather than re-offer the batch to a sink that is not answering"
    );
}

#[test]
fn every_ring_full_shed_marks_the_frame_unapplied() {
    let src = production("../core/src/websocket/pool_supervisor.rs");
    let accept = src
        .find("fn accept(&self, frame: Bytes) -> FrameSinkOutcome {")
        .expect("the frame sink's accept must exist");
    let body = &src[accept..];
    let body_end = body.find("\n    }\n").map_or(body.len(), |i| i + 6);
    let body = &body[..body_end];
    let sheds = body.matches("FrameSinkOutcome::RingFull").count();
    let marks = body.matches(".note_unapplied(seq)").count();
    assert!(
        sheds >= 3,
        "expected the three RingFull arms, found {sheds}"
    );
    assert_eq!(
        marks, sheds,
        "every RingFull return must be preceded by note_unapplied(seq): a shed frame exists ONLY in the WAL"
    );
    assert!(
        !body.contains("WalDropped =>")
            || !body
                .contains("note_unapplied(seq);\n            return FrameSinkOutcome::WalDropped"),
        "a WalDropped frame is not in the WAL and must not be marked unapplied"
    );
}

#[test]
fn the_segment_listing_excludes_the_open_segment() {
    let src = production("../storage/src/ws_frame_spill.rs");
    let listing = src
        .find("fn wal_segments_in(dir: &Path) -> Vec<PathBuf> {")
        .expect("wal_segments_in must exist");
    let body = &src[listing..listing + 900];
    assert!(
        body.contains("is_open_segment(p)"),
        "wal_segments_in must filter the writer's open segment, or a catch-up round can stage the file being appended to"
    );
    assert!(
        src.contains("set_open_segment(path);"),
        "open_new_segment must register the segment it opens"
    );
}

#[test]
fn both_sinks_advance_the_watermark_on_ack_and_on_rescue() {
    let ticks = production("../storage/src/tick_persistence.rs");
    let depth = production("../storage/src/depth_persistence.rs");
    assert!(
        ticks.contains(".note_ticks_acked(batch.max_seq)"),
        "tick sink ack"
    );
    assert!(
        depth.contains(".note_depth_acked(batch.max_seq)"),
        "depth sink ack"
    );
    assert!(
        ticks.matches("note_rescue_outcome_ticks(").count() >= 4,
        "tick rescues (sink ok, sink err, inline, rescue thread) must all report to the watermark"
    );
    assert!(
        depth.matches("note_rescue_outcome_depth(").count() >= 4,
        "depth rescues (sink ok, sink err, inline, rescue thread) must all report to the watermark"
    );
    for (name, src) in [("ticks", &ticks), ("depth", &depth)] {
        assert!(
            src.contains("wm.persist_if_due_now();"),
            "{name} sink must persist the watermark at its cadence — it is the only writer of the file mid-session"
        );
    }
}

#[test]
fn the_unfolded_frames_message_no_longer_claims_the_segments_were_archived() {
    let src = production("src/dhan_feed_stack.rs");
    let site = src
        .find("pub fn report_unfolded_wal_frames(")
        .expect("report_unfolded_wal_frames must exist");
    let body = &src[site..site + 3000];
    assert!(
        !body.contains("were already archived"),
        "boot stopped confirming on the lane's behalf on 2026-08-28; the message must not say the segments are gone"
    );
    assert!(
        body.contains("stay in the replay staging area"),
        "the message must say where the segments actually are"
    );
}

#[test]
fn a_refused_boot_replay_never_confirms_the_staged_leftovers() {
    let main = production("src/main.rs");
    assert!(
        main.contains(
            "ws_wal_replay_refused = batch.stopped_for_disk || batch.stopped_for_frame_cap"
        ),
        "STAGE-C must read the refusal flags off the batch, not infer 'nothing to replay' from an empty frame list"
    );
    assert!(
        main.contains("} else if ws_wal_replay_refused {"),
        "the confirm branch must be gated on the refusal — a refused pass leaves `replaying/` untouched"
    );
}

#[test]
fn both_lane_confirms_require_empty_producers_and_no_unlanded_batch() {
    let lane = production("src/dhan_feed_stack.rs");
    assert_eq!(
        lane.matches("replay_rows_landed(&mut ingest,").count(),
        2,
        "both confirm sites go through replay_rows_landed"
    );
    assert!(
        lane.contains("ingest.writer.pending() == 0 && ingest.depth_pending_rows() == 0"),
        "rows retained in a producer after a full queue are RAM-only and must block the confirm"
    );
    assert_eq!(
        lane.matches(".unlanded_total()").count(),
        2,
        "each refold snapshots the unlanded count before it folds"
    );
}

#[test]
fn the_drain_reset_is_bounded_by_the_ceiling_it_snapshotted_first() {
    let lane = production("src/dhan_feed_stack.rs");
    let snap = lane
        .find("let catchup_ceiling_seq = tickvault_storage::ws_frame_spill::current_frame_seq();")
        .expect("the catch-up snapshots the frame seq before its first round");
    let reset = lane
        .find("wm.reset_unapplied_below(catchup_ceiling_seq);")
        .expect("the reset is the bounded form");
    assert!(snap < reset, "snapshot before reset");
    assert!(
        !lane.contains("wm.reset_unapplied();"),
        "the unbounded reset would clear buckets of frames shed during the drain"
    );
}

#[test]
fn the_lane_declares_whether_a_depth_sink_exists() {
    let lane = production("src/dhan_feed_stack.rs");
    assert!(lane.contains("wm.mark_depth_tracked();"));
    assert!(lane.contains("wm.mark_depth_untracked();"));
}

#[test]
fn out_of_order_rescues_never_ack_the_watermark() {
    let ticks = production("../storage/src/tick_persistence.rs");
    let depth = production("../storage/src/depth_persistence.rs");
    for (name, src, f) in [
        ("ticks", &ticks, "note_rescue_outcome_ticks("),
        ("depth", &depth, "note_rescue_outcome_depth("),
    ] {
        let in_order = src
            .matches(&format!("{f}true, (batch.min_seq, batch.max_seq), true)"))
            .count()
            + src
                .matches(&format!("{f}false, (batch.min_seq, batch.max_seq), true)"))
                .count();
        assert_eq!(
            in_order, 2,
            "{name}: only the writer thread's own rescue is in order"
        );
        assert!(
            src.contains(&format!("{f}landed, range, false)")),
            "{name}: the inline producer rescue is out of order"
        );
        assert!(
            src.contains(&format!(
                "{f}landed, (batch.min_seq, batch.max_seq), false)"
            )),
            "{name}: the rescue thread is out of order"
        );
    }
}

/// The replay CONFIRM must consult `is_sink_suspect`, not only the drain.
///
/// # The seam this closes
///
/// Two mechanisms guard replayed rows and, until 2026-09-05, they disagreed
/// about the same event.
///
/// `note_ticks_acked` parks an ack into `suspect_max_ticks` instead of
/// advancing `hwm` while the sink is suspect, because a WAL-SUSPENDED QuestDB
/// table keeps ACKing ILP writes while silently not applying them — the
/// measured 2026-08-25 lie, 26 tables at once. That protects the SKIP
/// decision.
///
/// It did not protect the ARCHIVE decision. `wait_for_offload_drained` reads
/// `unlanded` and the completed counters and nothing else, and
/// `is_sink_suspect` existed with ZERO production callers. On a boot into that
/// state the ILP write returns `Ok`, no rescue runs, `unlanded` never moves,
/// `ticks_completed` advances, the wait returns true — and `confirm_replayed`
/// renames every staged segment into `archive/`, which is never re-globbed.
/// The rows are then in no table, no spill file and no `replaying/` directory:
/// permanent loss, on exactly the boot that most needs the WAL.
///
/// A source scan rather than a behavioural test because the failure is an
/// ABSENT call — the code that loses the data is the code that is not there,
/// and no runtime assertion can see a check nobody wrote.
#[test]
fn the_replay_confirm_refuses_a_suspect_sinks_ack() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs"),
    )
    .expect("dhan_feed_stack.rs must be readable");

    // Strip line comments FIRST.
    //
    // The first version of this test did not, and failed on its own subject:
    // the gate's doc comment NAMES `wait_for_offload_drained` while explaining
    // why the suspect check precedes it, so the ordering assertion found the
    // prose rather than the call. That is the identical defect repaired in
    // `wal_replay_confirm_symmetry_guard` earlier the same day — a source scan
    // that reads comments as code reports on the documentation.
    let stripped: String = src
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let src = stripped;

    let start = src
        .find("fn wal_replay_acked(")
        .expect("wal_replay_acked must exist — it is the only confirm gate");
    let body = &src[start..];
    let end = body.find("\nfn ").map_or(body.len(), |i| i + 1);
    let body = &body[..end];

    let suspect = body.find("is_sink_suspect()").expect(
        "wal_replay_acked must consult `is_sink_suspect()`. Without it a \
             WAL-suspended QuestDB's ACK is taken for a landing and \
             `confirm_replayed` archives segments whose rows were never \
             applied — into `archive/`, which is never re-globbed.",
    );
    let drained = body.find("wait_for_offload_drained").expect(
        "wal_replay_acked must still wait for the offload drain — the suspect \
         check replaces nothing, it is an ADDITIONAL refusal",
    );
    assert!(
        suspect < drained,
        "the suspect check must come BEFORE the drain wait. After it, a \
         suspect sink still spends the full wait and can still return true on \
         the completed counters — the refusal has to short-circuit."
    );
    assert!(
        body[..suspect].contains("return false")
            || body[suspect..].starts_with("is_sink_suspect()"),
        "the suspect arm must lead to a refusal, not a log-and-continue"
    );

    // The refusal must be distinguishable from the timeout arm in triage: the
    // two have the same OUTCOME (segments stay in `replaying/`) and completely
    // different causes — slow writers versus writers whose "ok" is a lie.
    assert!(
        body.contains(r#"source = "replay_sink_suspect""#),
        "the suspect refusal needs its own `source` field — sharing the \
         timeout's would make a lying sink read as a slow one"
    );
    assert!(
        body.contains("WAL_REPLAY_SINK_SUSPECT_REFUSALS_COUNTER"),
        "the refusal must be counted; an unrecorded refusal looks identical to \
         a boot with nothing to replay"
    );
}

/// The suspect-refusal counter must be SEEDED, like every other loss series.
#[test]
fn the_sink_suspect_refusal_counter_is_seeded_at_zero() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../storage/src/ws_frame_spill.rs"),
    )
    .expect("ws_frame_spill.rs must be readable");
    assert!(
        src.contains("metrics::counter!(WAL_REPLAY_SINK_SUSPECT_REFUSALS_COUNTER).increment(0)"),
        "seed the counter at construction. An unincremented counter is ABSENT \
         from the exporter, not zero, and an absent series reads as health — \
         when it may mean the single refusal is the only sample there will \
         ever be."
    );
}
