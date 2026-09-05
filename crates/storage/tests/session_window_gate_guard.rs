//! Ratchet: the 09:00–15:39 session-window gate stays wired, and stays OFF the
//! loss path.
//!
//! Operator requirement, 2026-09-05: every persisted row's `ts` AND
//! `received_at` must fall inside 09:00–15:39:59 IST.
//!
//! ## What this pins, and why a source scan
//!
//! Two constants have named this window for months —
//! `TICK_PERSIST_START_SECS_OF_DAY_IST` and `TICK_PERSIST_END_SECS_OF_DAY_IST`
//! — and `dhan_feed_stack.rs` records in its own words that **neither had a
//! reader on any write path**: "there is no persistence window GATE on this
//! lane at all … A row outside the window is written because NOTHING STOPS THE
//! WRITER". A window that exists only as a constant is documentation, and the
//! way it stayed documentation for months is that nothing failed when it was.
//!
//! ## The placement is the load-bearing part
//!
//! The gate MUST sit between `from_parsed_tick` and `append_row`, and it MUST
//! return `Ok`. Both halves matter, and they fail differently:
//!
//! * Inside `from_parsed_tick` (i.e. as a `TickRowError`): every `Err` from
//!   that constructor is treated by `append_tick_with_seq` as a LOSS — it calls
//!   `note_unapplied` and increments `tv_ticks_dropped_total`, which
//!   `dhan_ticks_dropped` PAGES on. The operator would be paged on every
//!   ordinary pre-open tick, and the one alarm that reports real tick loss
//!   would be trained into noise.
//! * Before `from_parsed_tick`: the check would run on the raw `ParsedTick`,
//!   testing a different pair of numbers than the ones that reach the table —
//!   the sentinel-LTT fallback makes `ts` become the receipt, and only the
//!   built row knows that.

use tickvault_common::source_scan::strip_rust_comments;

fn tick_persistence_src() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tick_persistence.rs");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    // The comments here DISCUSS the gate at length; scanning raw text would
    // let a deleted gate pass on the strength of the comment explaining it.
    strip_rust_comments(&raw)
}

/// The body of `append_tick_with_seq`, comments stripped.
fn append_fn_body(src: &str) -> String {
    let start = src
        .find("pub fn append_tick_with_seq")
        .expect("append_tick_with_seq is gone from tick_persistence.rs");
    let rest = &src[start..];
    let end = rest
        .find("\n    pub fn append_row")
        .unwrap_or(rest.len().min(6000));
    rest[..end].to_string()
}

#[test]
fn the_session_window_gate_is_wired_into_the_tick_write_path() {
    let src = tick_persistence_src();
    let body = append_fn_body(&src);

    assert!(
        body.contains("session_window::verdict("),
        "the session-window gate is GONE from `append_tick_with_seq`. Rows \
         outside 09:00-15:39:59 IST would be written again, and editing \
         TICK_PERSIST_* would not stop them -- those constants had no reader on \
         this path before this gate existed."
    );
    assert!(
        body.contains("is_refusal()"),
        "the gate no longer acts on its verdict. Computing a verdict and \
         ignoring it is worse than no gate: the code reads as guarded."
    );
}

#[test]
fn a_refused_row_is_not_reported_as_a_loss() {
    let src = tick_persistence_src();
    let body = append_fn_body(&src);

    let refuse_at = body
        .find("is_refusal()")
        .expect("gate missing -- covered by the test above");
    let arm = &body[refuse_at..];
    let arm_end = arm.find("self.append_row(&row)").unwrap_or(arm.len());
    let refusal_arm = &arm[..arm_end];

    assert!(
        refusal_arm.contains("return Ok(())"),
        "the refusal arm must `return Ok(())`. Returning `Err` routes it into \
         the loss block below, which calls `note_unapplied` and increments \
         tv_ticks_dropped_total -- an ALARMED metric. Every ordinary pre-open \
         tick would page the operator."
    );
    assert!(
        !refusal_arm.contains("note_unapplied"),
        "the refusal arm must NOT call `note_unapplied`. The frame is handled, \
         not deferred: re-offering it on the next replay would refuse it again, \
         forever, and hold the applied watermark back."
    );
    assert!(
        !refusal_arm.contains("tv_ticks_dropped_total"),
        "a window refusal is NOT a drop. `tv_ticks_dropped_total` means rows \
         left the fold and are gone; this means we deliberately declined to \
         write them. Mixing them makes the loss alarm unreadable."
    );
}

#[test]
fn the_gate_reads_the_built_row_not_the_raw_tick() {
    let src = tick_persistence_src();
    let body = append_fn_body(&src);

    // Order matters: from_parsed_tick -> verdict -> append_row.
    let build = body
        .find("TickRow::from_parsed_tick")
        .expect("build site gone");
    let gate = body.find("session_window::verdict(").expect("gate gone");
    let append = body
        .find("self.append_row(&row)")
        .expect("append site gone");

    assert!(
        build < gate && gate < append,
        "the gate must sit BETWEEN row construction and append (found build={build}, \
         gate={gate}, append={append}). Before construction it would test the raw \
         ParsedTick, which carries different numbers than the row: on a \
         sentinel-LTT tick the row's `ts` becomes the RECEIPT, and only the \
         built row knows that."
    );
    assert!(
        body.contains("row.ts_ist_nanos") && body.contains("row.received_at_ist_nanos"),
        "the gate must read BOTH stamps off the built row. The operator's rule \
         is that `ts` AND `received_at` are in window -- checking one is half a \
         gate wearing a whole gate's name."
    );
}

#[test]
fn the_refusal_counter_is_seeded_for_every_reason_label() {
    let src = tick_persistence_src();

    assert!(
        src.contains("TICK_OUT_OF_WINDOW_COUNTER"),
        "the refusal counter constant is gone; a refusal would be invisible."
    );
    let seed = src
        .find("fn register_drop_baseline")
        .expect("register_drop_baseline gone");
    let seed_body = &src[seed..(seed + 4000).min(src.len())];
    assert!(
        seed_body.contains("TICK_OUT_OF_WINDOW_COUNTER"),
        "the refusal counter is not SEEDED in register_drop_baseline. The \
         CloudWatch agent computes counters as deltas and DROPS the first \
         sample of a series it has never seen -- so an unseeded counter \
         publishes nothing on the one occasion it fires. This is the exact \
         defect that made 104,540 depth rows unclassifiable on 2026-08-28."
    );
    assert!(
        seed_body.contains("TsOutOfWindow") && seed_body.contains("ReceivedAtOutOfWindow"),
        "BOTH reason labels must be seeded -- a label set is a separate series, \
         so seeding one leaves the other's first increment to be swallowed."
    );
}
