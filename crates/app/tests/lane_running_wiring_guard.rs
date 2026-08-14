//! Ratchet: the Dhan lane's operator-facing "am I running" flag must have a
//! REAL production call site, and must be cleared as well as set.
//!
//! ## The defect this exists to prevent (2026-08-14)
//!
//! `FeedRuntimeState::set_dhan_lane_running` shipped with **zero production
//! callers**. Eight matches existed repo-wide and every one of them was in a
//! test: four in `feed_toggle_lifecycle_guard.rs`, four in the accessor's own
//! `mod tests`. The flag is initialised `false`, so `feed_health` returned
//!
//! > `Degraded — "enabled, but the feed was not started at boot"`
//!
//! **unconditionally**: for a healthy lane, for a dead lane, and for a lane
//! that had never been configured. The operator console rendered that constant
//! as a diagnosis and the feeds page prescribed a restart from it.
//!
//! This is the false-OK class inverted — a permanent NOT-OK is exactly as
//! useless as a permanent OK, because in neither case does the signal carry
//! information. The unit tests all passed the whole time: they called the
//! setter themselves, so they proved the setter worked while proving nothing
//! about whether anything used it.
//!
//! That is the specific weakness this guard closes. A test that exercises a
//! function cannot tell you the function is wired; only a scan for a call site
//! outside test code can.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under a crate's `src/` (recursively). Test directories are
/// deliberately excluded — a call site inside `tests/` is precisely what this
/// guard refuses to accept as evidence.
fn production_sources(crate_src: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![crate_src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    out
}

/// Strip `#[cfg(test)]` modules so an inline test call site cannot satisfy the
/// guard. Brace-counting from the `mod tests` that follows the attribute is
/// enough here: these modules are the last item in their file and are not
/// nested inside another `cfg(test)` block.
fn strip_cfg_test_modules(text: &str) -> String {
    let Some(idx) = text.find("#[cfg(test)]") else {
        return text.to_string();
    };
    text[..idx].to_string()
}

fn app_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn set_dhan_lane_running_has_a_production_call_site() {
    let mut sites: Vec<String> = Vec::new();
    for path in production_sources(&app_src()) {
        let text = strip_cfg_test_modules(&fs::read_to_string(&path).unwrap_or_default());
        if text.contains("set_dhan_lane_running(") {
            sites.push(path.display().to_string());
        }
    }

    assert!(
        !sites.is_empty(),
        "`set_dhan_lane_running` has NO production call site in crates/app/src.\n\
         That is the exact 2026-08-14 defect this guard exists to prevent: the flag \
         stayed `false` forever, so the operator console reported \"enabled, but the \
         feed was not started at boot\" for a HEALTHY lane, a DEAD lane, and an \
         ABSENT lane identically.\n\
         A status flag nothing writes is not a status flag. Wire it where the lane \
         actually comes up and goes down."
    );
}

#[test]
fn the_lane_both_sets_and_clears_its_running_flag() {
    let stack = app_src().join("dhan_feed_stack.rs");
    let text =
        strip_cfg_test_modules(&fs::read_to_string(&stack).expect("read dhan_feed_stack.rs"));

    assert!(
        text.contains("set_dhan_lane_running(true)"),
        "the Dhan lane never reports itself UP — `set_dhan_lane_running(true)` is \
         missing from dhan_feed_stack.rs"
    );
    assert!(
        text.contains("set_dhan_lane_running(false)"),
        "the Dhan lane never reports itself DOWN — `set_dhan_lane_running(false)` is \
         missing from dhan_feed_stack.rs.\n\
         Setting the flag without ever clearing it just replaces a permanent NOT-OK \
         with a permanent OK, which is strictly worse: the operator would be told the \
         lane is carrying data after every socket had died."
    );
}

#[test]
fn the_up_gauge_is_cleared_outside_the_error_arm() {
    // The 2026-08-14 sibling defect: `tv_dhan_feed_stack_up` was set to 0.0
    // ONLY inside `if let Err(err) = drain.await`. A drain that returns
    // NORMALLY -- which is what happens once every socket has died and the
    // ring closes -- left the gauge pinned at 1.0 for the life of the process.
    // The one metric whose job is "the lane is carrying data" reported a
    // healthy lane precisely when there were no sockets left.
    let stack = app_src().join("dhan_feed_stack.rs");
    let text =
        strip_cfg_test_modules(&fs::read_to_string(&stack).expect("read dhan_feed_stack.rs"));

    let clear_idx = text
        .find("FEED_STACK_UP_GAUGE).set(0.0)")
        .expect("the up-gauge is never cleared anywhere in the lane");
    let err_arm_idx = text
        .find("if let Err(err) = drain_outcome")
        .expect("the drain-outcome error arm is missing — this guard needs updating");

    assert!(
        clear_idx < err_arm_idx,
        "`tv_dhan_feed_stack_up` is cleared only INSIDE the drain's error arm.\n\
         A drain that exits normally (every socket dead, ring closed) then leaves the \
         gauge at 1.0 forever, so the lane reports itself healthy with nothing \
         connected. Clear it on every exit path, before the error arm."
    );
}
