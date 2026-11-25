//! Ratchet: the shutdown marks the seal writer's crash marker clean only after
//! the escalation queue's drain, and it does so whatever that drain's outcome
//! (audit PR31b-1, 2026-10-01).
//!
//! The marker (`seal-unwritten.mark`) tells the next boot whether the previous
//! process died holding sealed candles in memory. Until this change the seal
//! writer marked it clean at the end of its own final drain, while the
//! escalation queue, which drains AFTER it, could still hold up to 250,000
//! seals: a kill during that wait lost them behind a clean marker, and the
//! next boot reported nothing. The behaviour is tested in
//! `seal_writer_loop::pr31b1_tests`; this scan pins the ORDER in `main.rs`,
//! which no unit test can see.

use std::path::{Path, PathBuf};

fn main_rs() -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).expect("read main.rs")
}

/// Byte offset of `needle` in `haystack`, or a test failure naming it.
fn position(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("main.rs no longer contains {needle:?}"))
}

#[test]
fn the_marker_is_finished_after_the_escalation_drain_and_before_the_wal_floor() {
    let src = main_rs();
    let escalation = position(&src, "let escalation_handle = SEAL_ESCALATION_THREAD");
    let abandoned = position(&src, "seals_escalation_abandoned = seals_abandoned;");
    let finish = position(&src, "finish_unwritten_mark_at_shutdown(");
    let wal = position(&src, "// 5b-3. WAL spill final drain");
    assert!(
        escalation < abandoned && abandoned < finish && finish < wal,
        "the crash marker must be finished after the escalation step and before the WAL floor"
    );
    assert_eq!(
        src.matches("finish_unwritten_mark_at_shutdown(").count(),
        1,
        "exactly one finish, outside every escalation outcome branch"
    );
}

#[test]
fn the_finish_is_not_inside_the_escalation_outcome_branches() {
    // Inside the `if let Some(handle)` block the finish would be skipped when
    // no escalation thread was spawned (it falls back inline), and inside one
    // outcome arm it would be skipped for the others.
    let src = main_rs();
    let finish = position(&src, "finish_unwritten_mark_at_shutdown(");
    let step = position(&src, "// 5b-2c. Crash marker");
    assert!(step < finish, "the finish sits under its own step heading");
    let between = &src[step..finish];
    assert!(
        !between.contains("if let Some(handle)"),
        "the finish must not depend on the escalation thread existing"
    );
    // Moving the whole step, heading included, into one outcome arm would pass
    // the checks above. Every brace opened between the escalation block and the
    // finish must be closed again before the step heading, so the step sits at
    // the same depth as the escalation block, not inside it. (The step's own
    // `if let Some(dir)` opens one brace after the heading.)
    let block = position(&src, "if let Some(handle) = escalation_handle {");
    let before_step = &src[block..step];
    let opens = before_step.matches('{').count();
    let closes = before_step.matches('}').count();
    assert_eq!(
        opens, closes,
        "the crash-marker step must follow the escalation block, outside every outcome arm"
    );
}

#[test]
fn the_finish_is_bounded_like_every_other_shutdown_wait() {
    let src = main_rs();
    let step = position(&src, "// 5b-2c. Crash marker");
    let wal = position(&src, "// 5b-3. WAL spill final drain");
    let body = &src[step..wal];
    assert!(body.contains("tokio::task::spawn_blocking("));
    assert!(body.contains("tokio::time::timeout(SEAL_UNWRITTEN_MARK_FINISH_BUDGET"));
}

#[test]
fn the_marker_directory_is_recorded_where_the_writer_is_spawned() {
    let src = main_rs();
    let record = position(
        &src,
        "SEAL_UNWRITTEN_MARK_DIR.set(runner.spill_dir().to_path_buf())",
    );
    let spawn = position(
        &src,
        "run_seal_writer_loop(runner, seal_drain_interval(), cancel_rx)",
    );
    assert!(
        record < spawn,
        "the directory is read from the runner before the runner moves into the task"
    );
}
