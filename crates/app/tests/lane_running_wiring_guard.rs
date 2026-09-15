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

/// Keep the prefix before the first top-level test module. Inline test probes
/// and top-level test constants do not end production code. These source files
/// put production wiring before their test modules, so cutting at the module
/// excludes test call sites without hiding the rest of a production function.
fn strip_cfg_test_modules(text: &str) -> String {
    let Some((idx, _)) = text
        .match_indices("#[cfg(test)]\nmod ")
        .find(|(idx, _)| *idx == 0 || text.as_bytes()[*idx - 1] == b'\n')
    else {
        return text.to_string();
    };
    text[..idx].to_string()
}

#[test]
fn source_scan_keeps_production_after_inline_test_attributes() {
    let fixture = concat!(
        "fn drain() {\n    #[cfg(test)]\n    let probe = 0;\n}\n",
        "#[cfg(test)]\nconst TEST_CLOCK: u64 = 0;\n",
        "fn production_tail() {}\n",
        "#[cfg(test)]\nmod tests { fn test_only_tail() {} }\n",
    );
    let production = strip_cfg_test_modules(fixture);
    assert!(production.contains("fn production_tail() {}"));
    assert!(!production.contains("fn test_only_tail() {}"));
    assert_eq!(
        strip_cfg_test_modules("fn production_only() {}"),
        "fn production_only() {}"
    );
    assert!(strip_cfg_test_modules("#[cfg(test)]\nmod tests {}\n").is_empty());
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

    // The window that matters is between awaiting the drain and inspecting its
    // outcome. A clear anywhere else does not prove the normal-exit path is
    // covered.
    //
    // The first version of this test used `text.find(...)` for the clear and
    // compared it against the error arm's index. That was VACUOUS: `find`
    // returns the FIRST match in the file, and two unrelated `set(0.0)` calls
    // already precede this function entirely (one inside the drain, one at the
    // top of `run_dhan_feed_stack`). Deleting the clear this test exists to
    // protect left the assertion passing. Caught by adversarial review the
    // same day it was written — a guard that cannot fail is worse than none,
    // because it advertises coverage it does not provide.
    let await_idx = text
        .find("let drain_outcome = drain.await;")
        .expect("the drain await is missing — this guard needs updating");
    let err_arm_idx = text
        .find("if let Err(err) = drain_outcome")
        .expect("the drain-outcome error arm is missing — this guard needs updating");
    assert!(
        await_idx < err_arm_idx,
        "guard assumption broken: the drain await must precede its outcome check"
    );

    let between = &text[await_idx..err_arm_idx];
    assert!(
        between.contains("FEED_STACK_UP_GAUGE).set(0.0)"),
        "`tv_dhan_feed_stack_up` is not cleared between awaiting the drain and inspecting \
         its outcome, which means it is cleared only INSIDE the error arm.\n\
         A drain that exits NORMALLY — which is exactly what happens once every socket has \
         died and the ring closes — then leaves the gauge pinned at 1.0 for the life of the \
         process, so the one metric whose job is \"the lane is carrying data\" reports a \
         healthy lane precisely when nothing is connected."
    );

    // Non-vacuity companion: the flag must be cleared in the same window, for
    // the same reason. Without this, the gauge could fall while the console
    // kept claiming the lane runs.
    assert!(
        between.contains("set_dhan_lane_running(false)"),
        "the lane's running flag is not cleared alongside the up-gauge — the metric would \
         fall while the operator console still reported the lane as running"
    );
}
