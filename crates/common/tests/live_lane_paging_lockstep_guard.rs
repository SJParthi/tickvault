//! Pins that the live lane's UNRECOVERABLE losses page a human.
//!
//! # Why this file exists separately from the visibility guard
//!
//! `dashboard_live_lane_visibility_guard` proves every published metric is
//! *shown* (its scope widened from lane-only to ALL selected names on
//! 2026-08-26; the FILE keeps its lane name so this cross-reference and its
//! git history stay intact). Showing is not telling. A dashboard is a pull surface: it reports
//! only to someone who has already decided to look, which by definition is not
//! the person asleep at 02:00 when the durable floor stops holding.
//!
//! # The distinction this guard encodes
//!
//! The lane's loss counters are not equally serious, and treating them as a
//! flat list is how the 2026-08-14 family ended up alarming the *recoverable*
//! half of the loss chain while the unrecoverable half stayed silent:
//!
//! | Counter | Where the frame is | Can it be recovered? |
//! |---|---|---|
//! | `tv_ticks_dropped_total` | in the write-ahead log | yes, in principle |
//! | `tv_dhan_ws_wal_dropped_total` | **nowhere** | **no** |
//!
//! A loss that can never be recovered, and that produces no second signal
//! anywhere else in the system, has to reach a human by itself. That is the
//! rule this guard makes mechanical.
//!
//! # Deliberately narrow
//!
//! It does NOT demand an alarm for every lane counter. Eleven pagers for one
//! subsystem trains an operator to ignore all of them, and most of the
//! remaining counters are downstream symptoms these alarms would already have
//! fired for. It demands alarms for the specific signals that are both
//! permanent and evidence-free.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("live-lane paging guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("live-lane paging guard: {rel} must be readable: {e}"))
}

const LANE_ALARMS: &str = "deploy/aws/terraform/live-lane-alarms.tf";
const ERRCODE_ALARMS: &str = "deploy/aws/terraform/error-code-alarms.tf";
const NOISE_LOCK: &str = ".claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md";

/// Signals that are BOTH permanent and evidence-free, so a dashboard alone is
/// not enough. Each entry names why it cannot wait for someone to look.
const MUST_PAGE: &[(&str, &str)] = &[
    (
        "tv_dhan_feed_stack_up",
        "the lane is not carrying data at all — no candles, and the 15:31 \
         cross-verification has nothing on its live side",
    ),
    (
        "tv_dhan_ws_wal_dropped_total",
        "capture-at-receipt did not hold. The frame was never written anywhere, \
         so no replay, backfill or cross-verification can recover it, and there \
         is no second signal for this condition in the system",
    ),
    (
        "tv_ticks_dropped_total",
        "rows discarded on a flush failure — the largest single tick-loss window \
         in the lane",
    ),
    (
        "tv_dhan_ws_park_total",
        "a socket parked PERMANENTLY and will not dial again this session; the \
         pool silently carries fewer connections than planned until a restart",
    ),
    // Added 2026-08-19 (operator Quote 17: "no ticks loss"). This is the LAST
    // unwatched arm of the durable floor — `ws_frame_spill` shedding a frame
    // because the spill channel was full or the writer task was dead. The
    // bytes were never written anywhere, so it is the same unrecoverable class
    // as tv_dhan_ws_wal_dropped_total above.
    //
    // Deliberately NOT listed: `tv_ws_frame_spill_drop_critical`. Verified in
    // crates/storage/src/ws_frame_spill.rs (`SpillDropCounters`), it increments
    // on the SAME two arms as tv_ticks_lost_total and has no other production
    // emitter — the identical event counted twice. A second alarm would page
    // twice for one frame, which is the family-of-pagers pattern the noise lock
    // §2.3a argues against. tv_ticks_lost_total is the one kept because it also
    // carries the `source` label naming which arm shed the frame.
    (
        "tv_ticks_lost_total",
        "the spill writer shed the frame before it reached disk — no payload \
         survives to replay, backfill or cross-verify, and this is the last \
         arm of the durable floor that had no pager",
    ),
];

#[test]
fn every_unrecoverable_live_lane_loss_has_an_alarm() {
    let tf = read(LANE_ALARMS);

    let mut unpaged: Vec<&str> = Vec::new();
    for (metric, _why) in MUST_PAGE {
        if !tf.contains(&format!("metric_name         = \"{metric}\""))
            && !tf.contains(&format!("metric_name = \"{metric}\""))
        {
            unpaged.push(metric);
        }
    }

    assert!(
        unpaged.is_empty(),
        "these live-lane signals have NO alarm:\n  {}\n\nEach one is a permanent \
         loss with no second signal anywhere else — a dashboard reports it only \
         to someone who already decided to look, which is not the person asleep \
         when it happens. Add the alarm in {LANE_ALARMS}, or remove the entry \
         from MUST_PAGE in this file with a written reason.",
        unpaged.join("\n  ")
    );
}

#[test]
fn the_permanent_losses_never_send_a_false_recovery() {
    // `ok_actions = []` on the loss alarms is not a style choice. A counter's
    // delta returning to zero means no ADDITIONAL frames were lost — it can
    // never mean the lost ones came back. An OK page here would tell the
    // operator their data was recovered, which is the single most misleading
    // thing this system could say.
    let tf = read(LANE_ALARMS);

    for metric in [
        "tv_dhan_ws_wal_dropped_total",
        "tv_ticks_dropped_total",
        "tv_dhan_ws_park_total",
    ] {
        let Some(pos) = tf.find(&format!("\"{metric}\"")) else {
            panic!("{metric} is not alarmed — the previous test should have caught this");
        };
        // Scan the WHOLE resource block, not the remainder after the metric
        // name (2026-08-28). The old form anchored at the first mention of the
        // metric and read forward to the next `\n}\n`, which is correct for a
        // plain alarm — `ok_actions` follows the metric there — and WRONG for a
        // metric-math alarm, where the metric name lives inside a
        // `metric_query` block placed AFTER `ok_actions`. When `ticks_dropped`
        // became metric math on `dropped - spilled` this test failed on an
        // alarm that did set `ok_actions = []`, and the nested `}` of the
        // metric_query would have truncated the window anyway.
        //
        // The invariant is unchanged and is the point: a permanent-loss alarm
        // must never send a recovery page. Only the way the block is located
        // moved, so the guard reads metric-math and plain alarms alike.
        let block_start = tf[..pos]
            .rfind("\nresource \"aws_cloudwatch_metric_alarm\"")
            .unwrap_or_else(|| panic!("{metric} is mentioned outside any alarm resource"));
        let block_end = tf[block_start + 1..]
            .find("\nresource ")
            .map_or(tf.len(), |o| block_start + 1 + o);
        let block = &tf[block_start..block_end];
        assert!(
            block.contains("ok_actions = []"),
            "the alarm on {metric} does not set `ok_actions = []`. A recovery \
             page on a permanent-loss counter tells the operator the lost data \
             came back. It did not — the counter merely stopped rising"
        );
    }
}

#[test]
fn the_connected_but_silent_page_exists() {
    // The failure with NO other evidence: a subscribe that quietly did not take
    // leaves no payload to count, no parse to fail and no error of its own. The
    // socket is open, the lane gauge reads 1, and every loss counter sits at a
    // healthy flat zero. If the one detector for it is log-sink-only, the
    // detector is as invisible as the failure.
    let tf = read(ERRCODE_ALARMS);
    assert!(
        tf.contains("\"risk-gap-03\""),
        "RISK-GAP-03 has no error_code_alerts entry. The 30s silence scan is \
         then a detector nobody reads, watching a failure with no other trace"
    );
    assert!(
        tf.contains("RISK-GAP-03") && tf.contains("$.level = \\\"ERROR\\\""),
        "the risk-gap-03 filter must match the coded ERROR line"
    );
}

#[test]
fn the_withdrawn_dead_monitor_is_not_resurrected() {
    // `tv_dhan_feed_drain_respawn_total` was named in the 2026-08-14 family and
    // has ZERO emit sites, because the drain is not respawned at all — if it
    // dies the lane ends and the up-gauge falls. An alarm on it would be a
    // filter that can never match: permanently green, watching nothing. This
    // repo has already retired that exact shape twice (ws-reinject-01,
    // tick-conserve-01), which is why it is worth a test rather than a comment.
    let lane = read(LANE_ALARMS);
    let errcode = read(ERRCODE_ALARMS);
    for (file, body) in [(LANE_ALARMS, &lane), (ERRCODE_ALARMS, &errcode)] {
        assert!(
            !body.contains("metric_name         = \"tv_dhan_feed_drain_respawn_total\""),
            "{file} alarms tv_dhan_feed_drain_respawn_total, which has no emit \
             site anywhere in crates/*/src. The alarm would sit green forever \
             while watching nothing — see the §2.3a withdrawal in {NOISE_LOCK}"
        );
    }
}

#[test]
fn the_rule_file_records_the_authority() {
    // Rule-file-first: a new Dhan-scoped page is a REJECT under the noise
    // lock's §3 unless the dated authorization landed first. This asserts the
    // record exists rather than trusting that it was written.
    let lock = read(NOISE_LOCK);
    assert!(
        lock.contains("§2.3a"),
        "{NOISE_LOCK} carries no §2.3a section — the terraform in this PR adds \
         Dhan-scoped pages, which §3 REJECTs without a dated row first"
    );
    assert!(
        lock.contains("tv_dhan_ws_wal_dropped_total"),
        "§2.3a must name the metric it authorizes"
    );
    assert!(
        lock.contains("RISK-GAP-03"),
        "§2.3a must name the coded page it authorizes"
    );
}

#[test]
fn guard_is_not_vacuous() {
    // Every check above is a `contains` against a file that `read` would have
    // panicked on if missing. Prove the files are real and that MUST_PAGE is
    // not an empty list quietly passing everything.
    assert!(
        MUST_PAGE.len() >= 4,
        "MUST_PAGE has shrunk to {} entries — someone removed a signal from the \
         must-page set, which is exactly the change this guard should make \
         visible in a diff",
        MUST_PAGE.len()
    );
    for (metric, why) in MUST_PAGE {
        assert!(metric.starts_with("tv_"), "{metric} is not a metric name");
        assert!(
            why.len() > 40,
            "{metric} is in the must-page set with a {}-char reason. Write down \
             why it cannot wait for someone to open a dashboard",
            why.len()
        );
    }
    assert!(
        read(LANE_ALARMS).len() > 2000,
        "the lane alarm file is implausibly short — the scanner would pass \
         vacuously"
    );
}
