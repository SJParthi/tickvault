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
const USER_DATA: &str = include_str!("../../../deploy/aws/terraform/user-data.sh.tftpl");

/// Every writer that propagates a flush error while keeping its buffer.
///
/// 2026-08-21: this list held FOUR. `brutex_crossverify_persistence` was
/// removed along with the BruteX cross-verify comparator itself -- that
/// comparator read a table only the Groww feed could fill, so with one broker
/// it could only ever compare against nothing. A retention bound on a writer
/// that no longer exists is not coverage.
const BOUNDED_WRITERS: [(&str, &str); 3] = [
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
            ("deploy/aws/terraform/user-data.sh.tftpl", USER_DATA),
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
