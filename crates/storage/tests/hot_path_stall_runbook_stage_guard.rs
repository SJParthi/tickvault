//! Ratchet: the HOT-PATH-STALL-01 runbook names every hot-path stage.
//!
//! Why (B3 review, 2026-11-13). B3 added the never-paged stage
//! `drain_frame_post`, the only stage that times the frame arm's work after
//! the fold (the tick and depth hand-offs, a spare-less hand-off's reserve,
//! an inline rescue spill). The runbook the operator opens on a
//! `frame_drain` page did not mention it, nor `drain_main_tick`, so a stall
//! caused by a hand-off would be read as CPU or the fold. This test fails
//! when a stage exists that the runbook does not name, and pins the
//! post-fold triage line and the log-only p99 warn source.

#![cfg(test)]

use std::path::PathBuf;

use tickvault_storage::hot_path_telemetry::ALL_STAGES;

fn runbook() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/error-runbooks/hot-path-stall-error-codes.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The stage names a runbook text does not mention, as `` `name` ``.
fn stages_missing_from(text: &str) -> Vec<&'static str> {
    ALL_STAGES
        .iter()
        .map(|s| s.as_str())
        .filter(|name| !text.contains(&format!("`{name}`")))
        .collect()
}

#[test]
fn hot_path_stall_runbook_names_every_stage() {
    let text = runbook();
    let missing = stages_missing_from(&text);
    assert!(
        missing.is_empty(),
        "docs/error-runbooks/hot-path-stall-error-codes.md does not name the \
         stages {missing:?}; say whether each pages and how to read it"
    );
}

#[test]
fn hot_path_stall_runbook_triages_a_stall_after_the_fold() {
    let text = runbook();
    assert!(
        text.contains("tv_hot_path_stage_max_ns{stage=\"drain_frame_post\"}"),
        "the runbook must tell the operator to read the drain_frame_post \
         maximum when frame_drain stalls and drain_main_frame is small"
    );
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for false_phrase in POST_FOLD_FALSE_PHRASES {
        assert!(
            !flat.contains(false_phrase),
            "the runbook says `{false_phrase}`, which is false: the per-second \
             maximum is reset every publish and reads 0 while a step is blocked"
        );
    }
    let missing = post_fold_honesty_missing(&text);
    assert!(
        missing.is_empty(),
        "the runbook must keep the sentences containing {missing:?}: a stage \
         records only when its step ends, so an in-progress or wedged \
         hand-off shows only as a stale frame_drain heartbeat, never as a \
         large drain_frame_post"
    );
    assert!(
        text.contains("stage_p99_over_budget"),
        "the runbook must document the log-only p99 warn source \
         `stage_p99_over_budget` (same code, never paged)"
    );
}

/// B3 review (2026-11-13): `drain_frame_post` is recorded as the last step
/// of the frame arm, while the `frame_drain` heartbeat pages before the
/// next frame starts. So a hand-off still blocked when the page fires has
/// written no sample yet. These phrases (compared with whitespace collapsed,
/// so a re-wrap does not trip it) are the runbook's statement of that.
const POST_FOLD_HONESTY_PHRASES: &[&str] = &[
    "records only when the step ends",
    "Do not read a small `drain_frame_post` as \"not the hand-off\"",
    "a volume that stays wedged never produces that sample",
    // B3 review round 3: the post time is also in later frames' ring_dwell.
    "in the `ring_dwell` of every frame queued behind it, so do not add the two",
    // B3 review round 4: `publish_once` resets the per-second maximum every
    // second, so while a step is blocked the gauge reads 0, not the last
    // finished step (`test_publish_once_reads_0_for_a_second_with_no_sample`
    // in `hot_path_telemetry.rs` pins the code side).
    "reads 0 for every frame stage",
    "keeps the largest finished step for 75 s and then also reads 0",
];

/// Sentences that were false and must not come back (B3 review round 4).
const POST_FOLD_FALSE_PHRASES: &[&str] = &["its gauge still shows the last step that finished"];

/// The honesty phrases a runbook text lacks.
fn post_fold_honesty_missing(text: &str) -> Vec<&'static str> {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    POST_FOLD_HONESTY_PHRASES
        .iter()
        .copied()
        .filter(|p| !flat.contains(p))
        .collect()
}

#[test]
fn post_fold_honesty_check_bites_when_the_sentence_is_removed() {
    let text = runbook();
    assert!(post_fold_honesty_missing(&text).is_empty());
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let without = flat.replace("records only when the step ends", "records");
    assert_eq!(
        post_fold_honesty_missing(&without),
        vec!["records only when the step ends"]
    );
    let without_wedged = flat.replace("a volume that stays wedged never produces that sample", "");
    assert_eq!(
        post_fold_honesty_missing(&without_wedged),
        vec!["a volume that stays wedged never produces that sample"]
    );
    let without_overlap = flat.replace("so do not add the two", "");
    assert_eq!(
        post_fold_honesty_missing(&without_overlap),
        vec!["in the `ring_dwell` of every frame queued behind it, so do not add the two"]
    );
}

#[test]
fn stage_scanner_flags_a_runbook_without_the_new_stage() {
    let all: String = ALL_STAGES
        .iter()
        .map(|s| format!("`{}` ", s.as_str()))
        .collect();
    assert!(stages_missing_from(&all).is_empty());
    let without = all.replace("`drain_frame_post`", "");
    assert_eq!(stages_missing_from(&without), vec!["drain_frame_post"]);
}

fn repo_text(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// B3 review round 4 (2026-11-13): the capacity runbook and the CLAUDE.md
/// `hot_path_telemetry` row named only the 500 ms timer arm as drain work in
/// no stage. The silence scan (O(tracked)), the idle arm's catch-up seal and
/// seed steps, the catch-up, seed and shutdown arms and the frame's drop
/// after its record also hold the drain untimed, and a reader told "only
/// the timer arm" would chase the wrong subsystem. Every item must stay
/// named, in each document.
const UNTIMED_DRAIN_WORK_RUNBOOK: &[&str] = &[
    "the 500 ms flush timer arm",
    "the 30 s silence arm",
    "the idle arm",
    "the 5 s catch-up timer arm and the seed receive arm",
    "the shutdown arm",
    "the frame's drop after the record",
];

const UNTIMED_DRAIN_WORK_CLAUDE_MD: &[&str] = &[
    "the 500 ms flush timer arm",
    "the 30 s silence arm (`scan_silence_named`",
    "the idle arm (`step_idle_work`",
    "the catch-up timer and seed receive arms",
    "the shutdown arm",
    "the frame's own drop after the record",
];

/// The phrases `text` lacks, whitespace collapsed.
fn phrases_missing(text: &str, phrases: &[&'static str]) -> Vec<&'static str> {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    phrases
        .iter()
        .copied()
        .filter(|p| !flat.contains(p))
        .collect()
}

#[test]
fn every_untimed_drain_arm_is_named_where_the_post_stage_is_explained() {
    let runbook = repo_text("docs/runbooks/hot-path-capacity-proof.md");
    let missing = phrases_missing(&runbook, UNTIMED_DRAIN_WORK_RUNBOOK);
    assert!(
        missing.is_empty(),
        "docs/runbooks/hot-path-capacity-proof.md no longer names {missing:?} \
         as drain work in no stage"
    );
    let flat = runbook.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        !flat.contains("Still in no stage: the 500 ms timer arm's hand-offs"),
        "the capacity runbook is back to naming only the timer arm"
    );
    let claude_md = repo_text("CLAUDE.md");
    let missing = phrases_missing(&claude_md, UNTIMED_DRAIN_WORK_CLAUDE_MD);
    assert!(
        missing.is_empty(),
        "CLAUDE.md's hot_path_telemetry row no longer names {missing:?} as \
         drain work in no stage"
    );
    assert!(
        !claude_md.contains("Not covered: the timer arm's hand-offs (still `ring_dwell`)."),
        "CLAUDE.md is back to naming only the timer arm"
    );
}

#[test]
fn untimed_drain_work_check_bites_when_an_arm_is_cut() {
    let runbook = repo_text("docs/runbooks/hot-path-capacity-proof.md");
    let flat = runbook.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(phrases_missing(&flat, UNTIMED_DRAIN_WORK_RUNBOOK).is_empty());
    let cut = flat.replace("the 30 s silence arm", "");
    assert_eq!(
        phrases_missing(&cut, UNTIMED_DRAIN_WORK_RUNBOOK),
        vec!["the 30 s silence arm"]
    );
}
