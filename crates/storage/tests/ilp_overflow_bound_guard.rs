//! Pins that every ILP writer which RETAINS rows across a failed flush also
//! BOUNDS what it retains, and that the loss counters involved reach
//! CloudWatch.
//!
//! # The defect this prevents
//!
//! `questdb-rs`' `Sender::flush` is `flush_impl(buf, ..)?; buf.clear()`. The
//! `?` returns before the clear, so a failed flush keeps every row that did
//! not go out. That is deliberate — a two-second QuestDB reconnect should not
//! cost an audit row — and four writers in this crate relied on it with no
//! bound at all.
//!
//! Unbounded retention fails in two ways, both silent:
//!
//! * a sustained outage grows the buffer for as long as it lasts, because the
//!   caller appends the next row into the same buffer and flushes again;
//! * a SERVER-SIDE reject (a bad column type, a schema drift) is rejected
//!   identically on every retry, so the buffer is poisoned and the table is
//!   dead for the process lifetime — while the log line reads like a
//!   transient failure.
//!
//! Neither is visible from the call site: the code that leaks is the code that
//! is NOT there. A source scan is the only thing that can see an absence.

const OVERFLOW_SRC: &str = include_str!("../src/ilp_overflow.rs");
const AGENT_JSON: &str = include_str!("../../../deploy/aws/cloudwatch-agent.json");
/// The deployed CloudWatch agent config (was embedded in
/// `user-data.sh.tftpl` until 2026-08-25 — that duplicate pinned the template
/// at zero free bytes and was removed; user-data now copies this file after the
/// Step 5 clone).
const USER_DATA: &str = include_str!("../../../deploy/aws/cloudwatch-agent.json");

/// Every writer that propagates a flush error while keeping its buffer.
///
/// 2026-08-21: this list held FOUR. `brutex_crossverify_persistence` was
/// removed along with the BruteX cross-verify comparator itself -- that
/// comparator read a table only the Groww feed could fill, so with one broker
/// it could only ever compare against nothing. A retention bound on a writer
/// that no longer exists is not coverage.
const BOUNDED_WRITERS: [(&str, &str); 4] = [
    (
        "ws_event_audit_persistence",
        include_str!("../src/ws_event_audit_persistence.rs"),
    ),
    (
        "feed_episode_audit_persistence",
        include_str!("../src/feed_episode_audit_persistence.rs"),
    ),
    (
        "feed_scoreboard_persistence",
        include_str!("../src/feed_scoreboard_persistence.rs"),
    ),
    // 2026-09-05: found MISSING by `the_bounded_writer_list_names_every_writer_that_calls_the_bound`
    // on its first run. It had been calling `discard_if_overflowing` while absent
    // from this array, which is the proof the hardcoded list was not maintained.
    (
        "ws_connection_daily_persistence",
        include_str!("../src/ws_connection_daily_persistence.rs"),
    ),
];

#[test]
fn every_retaining_writer_bounds_what_it_retains() {
    let unbounded: Vec<&str> = BOUNDED_WRITERS
        .iter()
        .filter(|(_, src)| !src.contains("ilp_overflow::discard_if_overflowing"))
        .map(|(name, _)| *name)
        .collect();

    assert!(
        unbounded.is_empty(),
        "these ILP writers retain rows across a failed flush without bounding \
         the retention: {unbounded:?}\n\n\
         A sustained QuestDB outage grows their buffer with no cap, and a \
         SERVER-SIDE reject poisons it permanently — the same rows are \
         re-sent and re-rejected forever, so the table is dead for the process \
         lifetime while the log reads like a transient failure. Call \
         `ilp_overflow::discard_if_overflowing` from the flush-failure arm."
    );
}

/// Every flushing writer in the crate must DECLARE how it treats its buffer on
/// a failed flush — discovered, not listed.
///
/// # Why this exists beside the test above
///
/// That test iterates `BOUNDED_WRITERS`, a hardcoded array. Its name promises
/// "EVERY retaining writer", and an adversarial sweep on 2026-09-05 showed the
/// promise was not kept: seventeen files in `crates/storage/src` define a
/// flush, the array named three, and `ws_connection_daily_persistence` was
/// already calling `discard_if_overflowing` without being in it — proof the
/// list was not being maintained. A new `*_persistence.rs` that retained an
/// unbounded buffer across a failed flush would have been green forever.
///
/// A list cannot answer a question about a set it does not enumerate. So this
/// test enumerates the set and requires each member to fall into exactly one
/// of two documented treatments:
///
///   * **bounded retain** — calls `ilp_overflow::discard_if_overflowing`, so
///     the buffer survives a transient outage but cannot grow without limit;
///   * **discard** — calls `discard_pending`, so nothing is retained at all.
///
/// A writer with NEITHER retains without a bound, which is the defect. There
/// is deliberately no exemption list: the two treatments are exhaustive, and
/// an exemption would be the same staleness one level up.
#[test]
fn every_flushing_writer_declares_its_failure_treatment() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = std::fs::read_dir(&src_dir).expect("crates/storage/src must be readable");

    let mut flushing = 0usize;
    let mut bounded = 0usize;
    let mut undeclared: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with("_persistence.rs") {
            continue;
        }
        // Lossy: one bad byte must not silently drop a writer from the sweep.
        let bytes = std::fs::read(&path).expect("a listed source file must be readable");
        let body = String::from_utf8_lossy(&bytes);
        if !body.contains("fn flush") {
            continue;
        }
        flushing += 1;

        let bounded_retain = body.contains("discard_if_overflowing");
        let discards = body.contains("discard_pending");
        if bounded_retain {
            bounded += 1;
        }
        if !bounded_retain && !discards {
            undeclared.push(name.to_owned());
        }
    }

    assert!(
        flushing >= 15,
        "found only {flushing} flushing writers — the discovery is broken and \
         this test would pass vacuously"
    );
    assert!(
        bounded >= 3,
        "found only {bounded} bounded-retain writers — the discriminator is \
         broken; every writer would read as 'discards' and nothing would be \
         checked"
    );
    assert!(
        undeclared.is_empty(),
        "these ILP writers flush but declare NO treatment for a failed flush: \
         {undeclared:?}\n\n\
         A writer that neither bounds its retention \
         (`ilp_overflow::discard_if_overflowing`) nor drops it \
         (`discard_pending`) keeps appending into a buffer that a sustained \
         QuestDB outage grows without limit — and a server-side reject poisons \
         it permanently, so the same rows are re-sent and re-rejected for the \
         process lifetime while the log reads like a transient failure.\n\n\
         Pick one and call it from the flush-failure arm. There is no \
         exemption list here on purpose: the two treatments are exhaustive, \
         and an exemption would go stale exactly like the hardcoded array this \
         test was added to replace."
    );
}

/// `BOUNDED_WRITERS` must not go stale in the other direction either: every
/// file that actually calls the bound has to be in it.
///
/// This is the half that was already broken when the sweep looked —
/// `ws_connection_daily_persistence` called `discard_if_overflowing` and was
/// not listed, so the array had silently stopped describing the code.
#[test]
fn the_bounded_writer_list_names_every_writer_that_calls_the_bound() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = std::fs::read_dir(&src_dir).expect("crates/storage/src must be readable");

    let mut missing: Vec<String> = Vec::new();
    let mut found = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with("_persistence.rs") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("a listed source file must be readable");
        if !String::from_utf8_lossy(&bytes).contains("discard_if_overflowing") {
            continue;
        }
        found += 1;
        let stem = name.trim_end_matches(".rs");
        if !BOUNDED_WRITERS.iter().any(|(listed, _)| *listed == stem) {
            missing.push(stem.to_owned());
        }
    }

    assert!(
        found >= 4,
        "found only {found} writers calling the bound — the discovery is broken"
    );
    assert!(
        missing.is_empty(),
        "these writers call `discard_if_overflowing` but are NOT in \
         BOUNDED_WRITERS: {missing:?}\n\n\
         The array has stopped describing the code, so the test that iterates \
         it is checking a subset while its name promises every writer. Add \
         them."
    );
}

#[test]
fn the_bound_discards_loudly_rather_than_silently() {
    assert!(
        OVERFLOW_SRC.contains("metrics::counter!(PENDING_DISCARDED_COUNTER"),
        "the overflow bound must COUNT what it discards. A bound that drops \
         rows silently trades an unbounded leak for an invisible loss, which \
         is the worse of the two — the leak at least shows up as memory."
    );
    assert!(
        OVERFLOW_SRC.contains("buffer.clear()"),
        "the bound must actually clear the buffer; resetting only the caller's \
         count would leave the poisoned rows in place AND lose the ability to \
         detect the next overflow."
    );
}

#[test]
fn the_loss_counters_reach_cloudwatch_via_both_selector_copies() {
    // `tv_depth_rows_dropped_total` and `tv_depth_persist_errors_total` were
    // emitted for months and shipped by nothing, while the rows-WRITTEN
    // counter beside them was shipped — so depth read healthy off-box while
    // its losses were unobservable. That asymmetry is the false-OK shape.
    for metric in [
        "tv_ilp_rows_discarded_total",
        "tv_depth_rows_dropped_total",
        "tv_depth_persist_errors_total",
    ] {
        for (label, src) in [
            ("deploy/aws/cloudwatch-agent.json", AGENT_JSON),
            ("deploy/aws/cloudwatch-agent.json", USER_DATA),
        ] {
            assert!(
                src.contains(metric),
                "`{metric}` is missing from {label}. It is emitted by production \
                 code and would be invisible off-box — a loss nobody can see is \
                 indistinguishable from no loss at all."
            );
        }
    }
}

#[test]
fn the_order_and_position_discard_counters_are_distinguishable() {
    // Both writers increment the SAME counter name. Without a label a lost
    // POSITION capture is reported as a lost ORDER capture, and the two have
    // different causes and different triage.
    let position = include_str!("../src/position_update_events_persistence.rs");
    let order = include_str!("../src/order_update_events_persistence.rs");
    assert!(
        position.contains(r#""kind" => "position""#),
        "the position writer must label its discards, or a position-capture \
         loss is indistinguishable from an order-capture loss in CloudWatch"
    );
    assert!(
        order.contains(r#""kind" => "order""#),
        "the order writer must carry the matching label — one side labelled \
         and the other bare folds into the same series and reads as a single \
         unlabelled total, which is the state this test exists to end"
    );
}
