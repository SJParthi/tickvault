//! DRAIN ARM TIMING GUARD (2026-11-16).
//!
//! On the box, 16 Nov: `ring_dwell` and `receipt_to_fold` read over 2 ms in
//! 292 of 376 minutes while `drain_main_frame` (max 312 µs) and
//! `drain_frame_post` (max 134 µs) stayed far below it, so frames were
//! waiting in the ring behind drain work no stage timed. Every non-frame arm
//! of `run_frame_drain`'s `select!` now opens with
//! `let <name> = StageTimer::start(HotPathStage::DrainArm<Kind>);`, one stage
//! per arm kind (`tickvault_storage::hot_path_telemetry::DRAIN_ARM_STAGES`),
//! so the minute table can name the arm that held the drain.
//!
//! This guard reads the production source and fails when:
//! * a non-frame arm does not start its own timer as its first statement;
//! * the timer is bound to `_` (dropped at once, so it times nothing);
//! * an arm times the wrong stage, or a stage is timed by two arms;
//! * a NEW arm appears that is neither timed nor exempted below with a
//!   reason;
//! * the set of stages the drain times differs from `DRAIN_ARM_STAGES`.
//!
//! Exempt arms (each with its reason): the frame arm (its own stages:
//! `drain_main_frame`, `drain_main_tick`, `receipt_to_fold`,
//! `drain_frame_post`), the ring-tail deadline (only `break`), the ring-tail
//! end check (one `is_empty`, shutdown only) and the shutdown arm (once per
//! process, and it ends the loop or the tail).
//!
//! The behaviour half is
//! `dhan_feed_stack::tests::drain_arm_timing_tests::test_regression_every_non_frame_drain_arm_records_its_own_stage`.
//!
//! HONEST LIMIT: a text scan of one function. It proves the timer is the
//! first statement of each arm, not that nothing slow runs before the arm's
//! body (the `select!` poll itself is untimed).

use std::path::PathBuf;

use tickvault_storage::hot_path_telemetry::DRAIN_ARM_STAGES;

/// (text in the arm header, the stage variant its timer must name).
const TIMED_ARMS: &[(&str, &str)] = &[
    ("flush_timer.tick()", "DrainArmFlush"),
    ("catchup_timer.tick()", "DrainArmCatchup"),
    ("seed_rx.recv()", "DrainArmSeed"),
    ("silence_timer.tick()", "DrainArmSilence"),
    ("ingest.idle_work_pending()", "DrainArmIdle"),
];

/// (text in the arm header, why it records no arm stage).
const EXEMPT_ARMS: &[(&str, &str)] = &[
    (
        "maybe_frame = rx.recv()",
        "the frame arm: timed by its own frame stages",
    ),
    (
        "&mut ring_tail_deadline",
        "ring-tail deadline: only `break`",
    ),
    (
        "consume_budget(), if shutting_down",
        "ring-tail end: one is_empty check, shutdown only",
    ),
    (
        "shutdown.notified()",
        "shutdown: once per process, ends the loop or starts the tail",
    ),
];

const IMPORT: &str =
    "use tickvault_storage::hot_path_telemetry::{Stage as HotPathStage, StageTimer};";

fn source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The text of `run_frame_drain`'s `select!` block (production code only).
fn select_block(src: &str) -> Option<&str> {
    let prod = src.split(concat!("#[cfg(", "test)]")).next()?;
    let drain = &prod[prod.find("async fn run_frame_drain(")?..];
    let open = drain.find("tokio::select! {")?;
    let body = &drain[open..];
    let mut depth = 0usize;
    for (i, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&body[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// One `select!` arm: its header line and the lines of its body.
struct Arm<'a> {
    header: &'a str,
    body: Vec<&'a str>,
}

/// Splits the block into arms. An arm header is a line at the arms' own
/// indentation (12 spaces) that is not a comment, not `biased;` and holds
/// `=>`; its body runs to the next arm header.
fn arms(block: &str) -> Vec<Arm<'_>> {
    let mut out: Vec<Arm<'_>> = Vec::new();
    for line in block.lines() {
        let at_arm_indent = line.starts_with("            ") && !line.starts_with("             ");
        let trimmed = line.trim();
        if at_arm_indent && !trimmed.starts_with("//") && trimmed.contains("=>") {
            out.push(Arm {
                header: trimmed,
                body: Vec::new(),
            });
        } else if let Some(arm) = out.last_mut() {
            arm.body.push(trimmed);
        }
    }
    out
}

/// The problems the drain source has, empty when every arm is right.
fn problems(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !src.contains(IMPORT) {
        out.push(format!("missing import `{IMPORT}`"));
    }
    let Some(block) = select_block(src) else {
        return vec!["run_frame_drain's select! block not found".to_string()];
    };
    let arms = arms(block);
    if arms.len() < TIMED_ARMS.len() + EXEMPT_ARMS.len() {
        out.push(format!(
            "found only {} select! arms; the parser is lost",
            arms.len()
        ));
    }
    let mut timed: Vec<&str> = Vec::new();
    for arm in &arms {
        if EXEMPT_ARMS.iter().any(|(h, _)| arm.header.contains(h)) {
            continue;
        }
        let Some((_, variant)) = TIMED_ARMS.iter().find(|(h, _)| arm.header.contains(h)) else {
            out.push(format!(
                "new drain arm `{}` neither times itself nor is exempted with a reason",
                arm.header
            ));
            continue;
        };
        let first = arm
            .body
            .iter()
            .find(|l| !l.is_empty() && !l.starts_with("//"))
            .copied()
            .unwrap_or("");
        let want = format!("= StageTimer::start(HotPathStage::{variant});");
        if !(first.starts_with("let ") && first.ends_with(&want)) {
            out.push(format!(
                "arm `{}` must open with `let <name> {want}`, found `{first}`",
                arm.header
            ));
            continue;
        }
        if first.starts_with("let _ =") {
            out.push(format!(
                "arm `{}` binds its timer to `_`, which drops it at once",
                arm.header
            ));
            continue;
        }
        if timed.contains(variant) {
            out.push(format!("stage {variant} is timed by two arms"));
        }
        timed.push(variant);
    }
    let mut want: Vec<String> = DRAIN_ARM_STAGES.iter().map(|s| format!("{s:?}")).collect();
    let mut got: Vec<String> = timed.iter().map(|s| (*s).to_string()).collect();
    want.sort();
    got.sort();
    if want != got {
        out.push(format!(
            "the drain times {got:?}, DRAIN_ARM_STAGES is {want:?}"
        ));
    }
    out
}

#[test]
fn every_non_frame_drain_arm_records_its_own_stage() {
    let found = problems(&source());
    assert!(found.is_empty(), "drain arm timing: {found:#?}");
}

#[test]
fn the_guard_bites_when_an_arm_loses_its_timer() {
    let src = source();
    let line = "let _arm = StageTimer::start(HotPathStage::DrainArmSilence);";
    assert!(src.contains(line), "the silence arm's timer line moved");
    let stripped = src.replacen(line, "", 1);
    let found = problems(&stripped);
    assert!(
        found.iter().any(|p| p.contains("silence_timer.tick()")),
        "removing the silence timer must be caught: {found:?}"
    );
}

#[test]
fn the_guard_bites_on_a_discarded_timer() {
    let src = source();
    let line = "let _arm = StageTimer::start(HotPathStage::DrainArmIdle);";
    assert!(src.contains(line), "the idle arm's timer line moved");
    let found = problems(&src.replacen(
        line,
        "let _ = StageTimer::start(HotPathStage::DrainArmIdle);",
        1,
    ));
    assert!(
        found.iter().any(|p| p.contains("drops it at once")),
        "a timer bound to `_` must be caught: {found:?}"
    );
}

#[test]
fn the_guard_bites_on_a_new_untimed_arm() {
    let src = source();
    let anchor = "            _ = catchup_timer.tick() => {";
    assert!(src.contains(anchor), "the catch-up arm header moved");
    let added = src.replacen(
        anchor,
        "            _ = new_timer.tick() => {\n                do_work();\n            }\n            _ = catchup_timer.tick() => {",
        1,
    );
    let found = problems(&added);
    assert!(
        found.iter().any(|p| p.contains("new_timer.tick()")),
        "a new untimed arm must be caught: {found:?}"
    );
}

#[test]
fn the_guard_bites_when_two_arms_time_one_stage() {
    let src = source();
    let line = "let _arm = StageTimer::start(HotPathStage::DrainArmCatchup);";
    assert!(src.contains(line), "the catch-up arm's timer line moved");
    let found = problems(&src.replacen(
        line,
        "let _arm = StageTimer::start(HotPathStage::DrainArmSeed);",
        1,
    ));
    assert!(
        found.iter().any(|p| p.contains("catchup_timer.tick()")),
        "an arm timing another arm's stage must be caught at that arm: {found:?}"
    );
}
