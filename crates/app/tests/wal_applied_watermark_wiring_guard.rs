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
            .find(&format!("wal_replay_acked({stage})"))
            .unwrap_or_else(|| panic!("the {stage} confirm must wait on wal_replay_acked"));
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
        src.contains("if !blocking_flush(|| wal_replay_acked(\"catchup\")) {"),
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
