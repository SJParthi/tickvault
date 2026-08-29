//! Wiring guard for the per-CONNECTION daily WebSocket rollup.
//!
//! Operator directive 2026-08-29 (typos preserved): *"see how will you always
//! montiro in a day si there a wesbcoekt dsiconnect or websocket reocnenct
//! happened for all the conecntons dude ... based on evry day we ened to
//! cpature this rpeicsley rigth dude"*.
//!
//! The rollup is the ONLY thing that turns "did connection 7 drop today?"
//! from a day-scan into a single keyed row read. It is also the easiest thing
//! in the system to unhook by accident: it writes its own table, no other
//! code reads it, and every existing test would stay green if its one call
//! site disappeared. This guard is what makes that removal fail the build.
//!
//! Source-scan by design — the alternative is a live QuestDB, which CI does
//! not have.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/app -> crates -> repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Strip `//` line comments so a mention inside prose can never satisfy a
/// wiring assertion. (A comment saying "we call run_ws_connection_rollup" is
/// exactly the false-OK this guard exists to prevent.)
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_daily_scoreboard_actually_calls_the_rollup() {
    let src = strip_line_comments(&read("crates/app/src/feed_scoreboard_boot.rs"));
    assert!(
        src.contains("ws_connection_rollup::run_ws_connection_rollup"),
        "run_feed_scoreboard no longer calls run_ws_connection_rollup — the \
         per-connection daily table would stop being written, and NOTHING \
         else would fail: no other code reads it. 'Did connection 7 drop \
         today?' silently returns to a day-scan."
    );
    assert!(
        src.contains("build_episode_day_sql(target_ist_day)"),
        "the rollup must be handed the day's episode SQL — without it the \
         rows carry raw event counts but no classified incidents or blame"
    );
}

#[test]
fn the_rollup_is_registered_as_a_module() {
    let src = read("crates/app/src/lib.rs");
    assert!(
        src.contains("pub mod ws_connection_rollup;"),
        "the rollup module is not declared — it would not compile into the binary"
    );
}

#[test]
fn the_rollup_reuses_the_single_shared_tally_rule() {
    // A second copy of the incident-classification rules is a second way to
    // disagree with the daily scoreboard about the same day. `fold_episode_into_tally`
    // documents that hazard for the two paths that already exist; this is the third.
    let src = strip_line_comments(&read("crates/app/src/ws_connection_rollup.rs"));
    assert!(
        src.contains("fold_episode_into_tally"),
        "the rollup must reuse fold_episode_into_tally, never re-implement \
         which episode kinds count as which incident"
    );
    for reimplemented in [
        "EPISODE_KIND_DISCONNECT =>",
        "EPISODE_KIND_STALL_RESTART =>",
        "EPISODE_KIND_PROCESS_DEATH =>",
    ] {
        assert!(
            !src.contains(reimplemented),
            "the rollup appears to re-implement episode classification \
             (`{reimplemented}`) instead of delegating to the shared rule"
        );
    }
}

#[test]
fn an_unreadable_event_body_aborts_instead_of_writing_zeros() {
    // The whole point of the table is that "no disconnects" and "we could not
    // tell" are different answers. Writing a zero-filled row on an unparsable
    // read would destroy that distinction — the 2026-08-28 absent-series
    // failure, re-created inside its own fix.
    let src = strip_line_comments(&read("crates/app/src/ws_connection_rollup.rs"));
    let parse_arm = src
        .split("fold_ws_event_rows(&mut folded, &ws_body)")
        .nth(1)
        .expect("the rollup must fold the ws_event body");
    let window: String = parse_arm.chars().take(1200).collect();
    assert!(
        window.contains("return None"),
        "an unparsable ws_event_audit body must abort the rollup, not write \
         a table of zeros that reads as a clean day"
    );
}

#[test]
fn the_never_appeared_case_is_reported_not_swallowed() {
    let src = strip_line_comments(&read("crates/app/src/ws_connection_rollup.rs"));
    assert!(
        src.contains("connections_never_appeared"),
        "a connection with classified incidents but no lifecycle event all \
         day is a finding — the two tables disagree about whether the socket \
         existed — and must be reported, never folded into a quiet success"
    );
}

#[test]
fn the_rollup_can_never_fail_the_scoreboard() {
    // It summarises two already-durable tables. A failure here must lose a
    // convenience view, never a verdict.
    let src = strip_line_comments(&read("crates/app/src/feed_scoreboard_boot.rs"));
    // Bound the window to the call EXPRESSION itself. Scanning to end of file
    // would sweep in the crate's own test module (thousands of lines, full of
    // `?`) and fail on prose rather than on the call — a guard that fails for
    // the wrong reason teaches the next reader to weaken it.
    let call: String = src
        .split("ws_connection_rollup::run_ws_connection_rollup")
        .nth(1)
        .expect("call site")
        .chars()
        .take(300)
        .collect();
    let before: String = src
        .split("ws_connection_rollup::run_ws_connection_rollup")
        .next()
        .unwrap_or_default()
        .chars()
        .rev()
        .take(80)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    assert!(
        before.contains("let _ ="),
        "the rollup's result must be discarded at the call site (`let _ =`) \
         so it can never short-circuit or fail run_feed_scoreboard; found \
         instead: ...{before}"
    );
    assert!(
        !call.contains('?'),
        "the rollup call must not propagate with `?` — a rollup failure would \
         abort the scoreboard verdict it is only supposed to annotate"
    );
}

#[test]
fn guard_self_test_comment_stripping_actually_bites() {
    // Without this, every assertion above could be satisfied by a comment.
    let src = "// run_ws_connection_rollup is called below\nlet x = 1;";
    let stripped = strip_line_comments(src);
    assert!(!stripped.contains("run_ws_connection_rollup"));
    assert!(stripped.contains("let x = 1;"));
}

// ---------------------------------------------------------------------------
// The per-TABLE storage measurement shares this guard file, because it shares
// the failure mode: it writes its own table, nothing else reads it, and every
// other test in the workspace would stay green if its one call site vanished.
// ---------------------------------------------------------------------------

#[test]
fn the_daily_scoreboard_actually_measures_per_table_storage() {
    let src = strip_line_comments(&read("crates/app/src/feed_scoreboard_boot.rs"));
    assert!(
        src.contains("table_storage_rollup::run_table_storage_rollup"),
        "run_feed_scoreboard no longer calls run_table_storage_rollup — per-table \
         disk attribution silently returns to being DERIVED from row widths, and \
         nothing else would fail: no other code reads that table."
    );
}

#[test]
fn the_storage_rollup_is_registered_as_a_module() {
    let src = read("crates/app/src/lib.rs");
    assert!(
        src.contains("pub mod table_storage_rollup;"),
        "the storage rollup module is not declared — it would not compile in"
    );
}

#[test]
fn the_storage_rollup_can_never_fail_the_scoreboard() {
    let src = strip_line_comments(&read("crates/app/src/feed_scoreboard_boot.rs"));
    let before: String = src
        .split("table_storage_rollup::run_table_storage_rollup")
        .next()
        .unwrap_or_default()
        .chars()
        .rev()
        .take(80)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    assert!(
        before.contains("let _ ="),
        "the storage rollup's result must be discarded at the call site so it can \
         never short-circuit run_feed_scoreboard; found instead: ...{before}"
    );
    // Window-bounded for the same reason as the connection rollup's check: an
    // end-of-file scan sweeps in the crate's own test module and fails on prose.
    let call: String = src
        .split("table_storage_rollup::run_table_storage_rollup")
        .nth(1)
        .expect("call site")
        .chars()
        .take(300)
        .collect();
    assert!(
        !call.contains('?'),
        "the storage rollup call must not propagate with `?` — a measurement \
         failure would abort the verdict it only annotates"
    );
}

#[test]
fn an_unreadable_table_is_recorded_unmeasured_never_as_zero_bytes() {
    // The discipline the whole module exists for: "we could not read it" and
    // "it takes no space" are different facts, and only one of them is safe to
    // infer from silence.
    let src = strip_line_comments(&read("crates/app/src/table_storage_rollup.rs"));
    assert!(
        src.contains("storage_row(table, probe, day_ist_midnight_nanos)"),
        "every table must go through storage_row, which turns an unreadable \
         probe into an unmeasured row with sentinels rather than a zero"
    );
    assert!(
        src.contains("table_storage_no_disk_size_column"),
        "a response carrying no `diskSize` column must be reported — otherwise \
         every future measurement is blind and nothing says so"
    );
}
