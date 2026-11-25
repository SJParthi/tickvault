//! Guard — the metrics recorder is installed BEFORE anything registers a metric.
//!
//! # The bug this makes extinct
//!
//! `metrics::counter!` resolved before a recorder is installed returns
//! `Counter { inner: None }` (metrics 0.24.6 `recorder/mod.rs::with_recorder`
//! falls through to `NOOP_RECORDER`, whose `register_counter` returns
//! `Counter::noop()`). `Counter::increment` is `if let Some(c) = &self.inner`,
//! so that handle is a **permanent no-op**. A caller that CACHES the handle —
//! which `SpillDropCounters` does deliberately, to keep the frame-drop path
//! allocation-free — is then dead for the entire process lifetime.
//!
//! It costs real alarms. Verified live on prod 2026-08-26:
//! `tv_ws_frame_spill_drop_critical` and `tv_ticks_lost_total` were **absent
//! from `/metrics` entirely** while their neighbours (registered after the
//! install) rendered at 0. That silently disarmed
//! `tv-<env>-ticks-lost-at-spill-writer` and one leg of the
//! `durable-floor-breach` composite — two of the four detectors the design
//! believes it has on the durable floor.
//!
//! # Why a guard and not just the fix
//!
//! This is the SECOND occurrence in `main.rs`. The 2026-07-14 PR-C3 note
//! records the identical no-op-recorder loss for `tv_ws_frame_wal_replay_total`,
//! repaired by moving the INCREMENT to a post-install site. Moving one
//! increment fixes one instance and leaves the ordering hazard intact — which
//! is exactly why it recurred with a different counter three weeks later.
//! Ordering that matters and is enforced by nothing is a bug waiting for its
//! next author.
//!
//! # Honest scope
//!
//! This is a SOURCE-ORDER scan of `main.rs`, so it constrains the boot flow of
//! that file and nothing else. It cannot see a registration reached through a
//! call chain from an earlier boot step, and it does not attempt to: the
//! failure mode it exists to stop is a construction sited literally above the
//! install, which is what happened twice.

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Byte offset of the first occurrence of `needle` OUTSIDE a `//` comment.
///
/// Comment-stripping matters here: both the fix and this guard are described
/// in prose directly above the code they describe, and a scan that counted
/// those mentions would match the documentation rather than the call.
fn first_code_offset(haystack: &str, needle: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in haystack.lines() {
        let code = line.split_once("//").map_or(line, |(before, _)| before);
        if let Some(col) = code.find(needle) {
            return Some(offset + col);
        }
        offset += line.len() + 1;
    }
    None
}

#[test]
fn init_metrics_runs_before_the_wal_spill_constructs_its_counters() {
    let src = main_rs();

    let install = first_code_offset(&src, "observability::init_metrics")
        .expect("main.rs must call observability::init_metrics");
    let spill =
        first_code_offset(&src, "WsFrameSpill::new").expect("main.rs must construct WsFrameSpill");

    assert!(
        install < spill,
        "observability::init_metrics (byte {install}) must run BEFORE \
         WsFrameSpill::new (byte {spill}).\n\
         \n\
         WsFrameSpill::new builds SpillDropCounters, which RESOLVES AND CACHES \
         metrics::Counter handles. Handles resolved before the recorder is \
         installed are permanent no-ops, so tv_ws_frame_spill_drop_critical and \
         tv_ticks_lost_total never emit and their alarms can never fire.\n\
         \n\
         This exact bug has occurred twice in this file. Do not fix it by moving \
         an increment — move the install."
    );
}

#[test]
fn init_metrics_runs_before_the_wal_replay_that_counts_recovered_frames() {
    // The 2026-07-14 instance. `replay_all` itself does not register, but the
    // increments derived from its result once did, and the note explaining
    // that still sits in the file. Pinning the ordering keeps the repaired
    // instance repaired even if those increments move back to their natural
    // site next to the code that produces the numbers.
    let src = main_rs();

    let install = first_code_offset(&src, "observability::init_metrics")
        .expect("main.rs must call observability::init_metrics");
    let replay = first_code_offset(&src, "ws_frame_spill::replay_all")
        .expect("main.rs must call replay_all");

    assert!(
        install < replay,
        "observability::init_metrics (byte {install}) must run BEFORE \
         replay_all (byte {replay}) — see the PR-C3 2026-07-14 note in main.rs \
         for what happens when it does not."
    );
}

#[test]
fn the_fix_is_documented_where_the_next_author_will_look() {
    // A pure ordering assertion tells a future reader THAT the order matters
    // and not WHY, and "why" is the half that stopped this being fixed
    // properly the first time. Pin that the reasoning travels with the code.
    let src = main_rs();
    let install = first_code_offset(&src, "observability::init_metrics")
        .expect("main.rs must call observability::init_metrics");

    let preamble = &src[..install];
    let window_start = preamble.len().saturating_sub(2_500);
    let window = &preamble[window_start..];

    assert!(
        window.contains("no-op"),
        "the init_metrics call site must carry the reason the ORDER matters \
         (a pre-install handle is a permanent no-op). Without it the next \
         author reads a relocated call with no explanation and moves it back."
    );
}

#[test]
fn guard_self_test_detects_the_broken_order() {
    // Bite-proof, both directions — a guard whose failure mode is never
    // exercised is a guard nobody knows works.
    let broken = "    let s = WsFrameSpill::new(&p);\n    observability::init_metrics(&c)?;\n";
    let fixed = "    observability::init_metrics(&c)?;\n    let s = WsFrameSpill::new(&p);\n";

    let bad_install = first_code_offset(broken, "observability::init_metrics").unwrap();
    let bad_spill = first_code_offset(broken, "WsFrameSpill::new").unwrap();
    assert!(
        bad_install > bad_spill,
        "self-test: the scanner must SEE the broken order"
    );

    let ok_install = first_code_offset(fixed, "observability::init_metrics").unwrap();
    let ok_spill = first_code_offset(fixed, "WsFrameSpill::new").unwrap();
    assert!(
        ok_install < ok_spill,
        "self-test: the scanner must accept the fixed order"
    );
}

#[test]
fn guard_self_test_ignores_mentions_inside_comments() {
    // The fix ships with a long comment naming both symbols. If the scanner
    // matched comments it would find `init_metrics` inside the STAGE-C prose
    // and pass a genuinely broken file.
    let commented = "    // observability::init_metrics must run first\n\
                         let s = WsFrameSpill::new(&p);\n\
                         observability::init_metrics(&c)?;\n";

    let install = first_code_offset(commented, "observability::init_metrics").unwrap();
    let spill = first_code_offset(commented, "WsFrameSpill::new").unwrap();
    assert!(
        install > spill,
        "self-test: a mention inside a `//` comment must NOT count as the call \
         site — otherwise the guard passes a file whose real order is wrong"
    );
}

/// Count occurrences of `needle` OUTSIDE `//` comments, across the whole file.
///
/// The sibling helper stops at the FIRST hit, which is what makes it right for
/// the ordering tests and blind to the defect below.
fn code_occurrences(haystack: &str, needle: &str) -> usize {
    haystack
        .lines()
        .map(|line| {
            let code = line.split_once("//").map_or(line, |(before, _)| before);
            code.matches(needle).count()
        })
        .sum()
}

/// `init_metrics` must be called EXACTLY ONCE.
///
/// This is a count, not an order, and the distinction is the entire point.
/// `PrometheusBuilder::install()` binds the exporter's HTTP listener, so a
/// second call binds the same port a second time in the same process and
/// returns EADDRINUSE; `main` propagates that with `?` and the binary cannot
/// start at all.
///
/// It happened. On 2026-08-26 the install was moved ahead of STAGE-C to fix a
/// recorder-ordering bug, and the move COPIED rather than MOVED -- leaving two
/// calls. Every ordering test in this file still passed, because the FIRST
/// call was in exactly the right place; ordering was never violated. The
/// binary shipped to prod on 2026-08-28, systemd retried it eight times in
/// eighteen seconds, gave up with "Start request repeated too quickly", and
/// the box sat with no app through the 09:15 market open.
///
/// The failure mode is worth naming because it wastes the responder's time:
/// EADDRINUSE reads as "another process has the port", so the instinct is to
/// hunt an orphan. There was none -- `ss`, `lsof` and `fuser` all reported
/// 9091 free while a fresh `systemctl start` still failed. A port that is
/// provably free and still collides means the process is colliding with
/// itself.
#[test]
fn init_metrics_is_called_exactly_once() {
    let src = main_rs();

    let calls = code_occurrences(&src, "observability::init_metrics");

    assert_eq!(
        calls, 1,
        "main.rs calls observability::init_metrics {calls} times; it must be \
         called EXACTLY ONCE.\n\n\
         init_metrics installs the Prometheus exporter, which BINDS an HTTP \
         listener. A second call binds the same port again in the same \
         process and fails with `Address in use (os error 98)`, which `?` \
         turns into a boot failure -- the binary cannot start at all.\n\n\
         If you are MOVING the call to fix an ordering problem, delete the \
         old one in the same edit. The ordering tests in this file cannot \
         catch a duplicate: they check where the FIRST call sits, and a \
         copy leaves that first call exactly where it belongs."
    );
}

/// Self-test: the count guard must actually bite on a duplicated call.
///
/// Without this, a helper that silently returned 1 would leave the guard
/// permanently green -- the same shape of false-OK the guard exists to stop.
#[test]
fn count_guard_self_test_detects_a_duplicated_call() {
    let one = "    observability::init_metrics(&cfg)?;\n";
    let two = format!("{one}{one}");

    assert_eq!(code_occurrences(one, "observability::init_metrics"), 1);
    assert_eq!(code_occurrences(&two, "observability::init_metrics"), 2);

    // ...and a commented-out mention must NOT count, or the guard would fire
    // on the prose that documents it.
    let commented = "    // observability::init_metrics is installed above\n";
    assert_eq!(
        code_occurrences(commented, "observability::init_metrics"),
        0
    );
}
