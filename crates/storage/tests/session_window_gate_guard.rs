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

    // Seeding moved OUT of `register_drop_baseline` on 2026-09-05 and INTO
    // `OutOfWindowCounters::new`, which is where the handles are built. The
    // move is the point of this assertion, not an incidental relocation: the
    // copy in `register_drop_baseline` listed only TWO of the three reasons —
    // `ts_out_of_plausible_band` was missing — so on a garbage epoch, the one
    // shape a band check exists for, the first increment would still have been
    // swallowed as the CloudWatch delta baseline. Two seed sites for one series
    // is how that happens: the second is written from memory of the first.
    // Seeding beside the handles cannot fall behind the reason vocabulary,
    // because it iterates it.
    let ctor = src
        .find("fn new(feed: Feed, counter: &'static str, reasons:")
        .expect("OutOfWindowCounters::new is gone -- the seed site with it");
    let ctor_body = &src[ctor..(ctor + 1200).min(src.len())];
    assert!(
        ctor_body.contains("increment(0)"),
        "the refusal handles are no longer seeded at 0. The CloudWatch agent \
         computes counters as deltas and DROPS the first sample of a series it \
         has never seen -- so an unseeded counter publishes nothing on the one \
         occasion it fires. This is the exact defect that made 104,540 depth \
         rows unclassifiable on 2026-08-28."
    );
    assert!(
        ctor_body.contains("make(reasons[0])")
            && ctor_body.contains("make(reasons[1])")
            && ctor_body.contains("make(reasons[2])"),
        "every reason label must get a seeded handle. A label set is a separate \
         series, so seeding two of three leaves the third's first increment to \
         be swallowed -- which is precisely what the old two-of-three seed loop \
         in register_drop_baseline did."
    );
    assert!(
        !src.contains("fn register_drop_baseline")
            || !src[src.find("fn register_drop_baseline").unwrap_or(0)
                ..(src.find("fn register_drop_baseline").unwrap_or(0) + 4000).min(src.len())]
                .contains("TICK_OUT_OF_WINDOW_COUNTER"),
        "the refusal counter is seeded in TWO places again. One of them will \
         fall behind the reason vocabulary -- the first one did, silently, \
         within a day. Seed beside the handles or not at all."
    );
}

// ---------------------------------------------------------------------------
// DEPTH
// ---------------------------------------------------------------------------

fn depth_persistence_src() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/depth_persistence.rs");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    strip_rust_comments(&raw)
}

/// The body of `DepthWriter::append_row`, comments stripped.
fn depth_append_body(src: &str) -> String {
    let start = src
        .find("pub fn append_row(&mut self, row: &DepthRow)")
        .expect("DepthWriter::append_row is gone from depth_persistence.rs");
    let rest = &src[start..];
    let end = rest
        .find("\n    fn append_row_inner")
        .unwrap_or(rest.len().min(6000));
    rest[..end].to_string()
}

#[test]
fn the_session_window_gate_is_wired_into_the_depth_write_path() {
    let src = depth_persistence_src();
    let body = depth_append_body(&src);

    assert!(
        body.contains("row_is_in_an_open_window"),
        "the session-window gate is GONE from `DepthWriter::append_row`. Depth \
         is 24x the tick row volume -- the largest payload in the process -- so \
         an ungated depth writer defeats the operator's window rule at the one \
         table where it costs the most disk."
    );
    assert!(
        body.contains("out_of_window.note("),
        "the depth gate no longer counts its refusals. A silent refusal and a \
         silent write are indistinguishable from outside the process."
    );
}

#[test]
fn a_refused_depth_row_is_not_reported_as_a_loss() {
    let src = depth_persistence_src();
    let body = depth_append_body(&src);

    let gate_at = body
        .find("out_of_window.note(")
        .expect("depth gate missing -- covered by the test above");
    let head = &body[..gate_at];
    let inner_at = body
        .find("self.append_row_inner(")
        .unwrap_or_else(|| panic!("depth append_row no longer calls append_row_inner"));

    assert!(
        gate_at < inner_at,
        "the depth gate must run BEFORE `append_row_inner` (found gate={gate_at}, \
         inner={inner_at}). After it, the row is already in the ILP buffer and \
         the refusal is theatre."
    );

    // Every refusal arm returns Ok. An Err from append_row is what the caller
    // turns into `note_unapplied` + tv_depth_rows_dropped_total, which is
    // ALARMED -- so an Err here pages the operator on every ordinary pre-open
    // depth frame and trains the one real depth-loss alarm into noise.
    // EVERY refusal arm returns Ok, not just one. The first version of this
    // assertion asked only whether `return Ok(())` appeared anywhere in the
    // region, and a bite test planted an `Err` in two of the three arms and
    // PASSED -- one surviving Ok was enough to satisfy it. A guard that can be
    // satisfied by the arm you did not break is decoration. So: no `return Err`
    // anywhere in the refusal region, and one `return Ok(())` per counted arm.
    let refusal_region = &body[gate_at..inner_at];
    let arms = refusal_region.matches("out_of_window.note(").count();
    let oks = refusal_region.matches("return Ok(())").count();
    assert!(
        arms > 0 && oks >= arms,
        "every depth window-refusal arm must `return Ok(())` -- found {arms} \
         refusal arm(s) but only {oks} Ok return(s). An Err is routed to \
         note_unapplied + tv_depth_rows_dropped_total by the caller, which is \
         the ALARMED loss path: the operator would be paged on every ordinary \
         pre-open depth frame, and the one alarm that reports real depth loss \
         would be trained into noise."
    );
    assert!(
        !refusal_region.contains("return Err"),
        "a depth window-refusal arm returns Err. See above -- that is the \
         alarmed loss path, and a deliberate refusal is not a loss."
    );
    assert!(
        !head.contains("note_unapplied") && !refusal_region.contains("note_unapplied"),
        "the depth refusal path must NOT call note_unapplied: re-offering the \
         frame on the next replay would refuse it again, forever, holding the \
         applied watermark back."
    );
}

#[test]
fn the_depth_gate_speaks_arrival_because_depth_has_one_clock() {
    let src = depth_persistence_src();

    // Not cosmetic. `market_depth` has exactly ONE clock: the designated `ts`
    // IS the receipt instant, because the depth protocol carries no exchange
    // timestamp field at all. Borrowing the tick vocabulary would tell an
    // operator the EXCHANGE stamped a row out of window, which is not a thing
    // depth can express -- a wrong cause on a true alarm.
    let start = src
        .find("DEPTH_OUT_OF_WINDOW_REASONS")
        .expect("depth reason vocabulary is gone");
    let block = &src[start..(start + 400).min(src.len())];
    assert!(
        block.contains("arrival_out_of_window")
            && block.contains("arrival_unknown")
            && block.contains("arrival_out_of_plausible_band"),
        "the depth reasons must all say `arrival_`. Depth has no exchange \
         clock, so `ts_out_of_window` would name a cause that cannot exist."
    );
    assert!(
        !block.contains("ts_out_of_window"),
        "a depth reason borrowed the tick vocabulary. See above: it names a \
         cause depth cannot have."
    );
}

// ---------------------------------------------------------------------------
// CANDLES
// ---------------------------------------------------------------------------

fn candle_writer_src() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shadow_candle_writer.rs");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    strip_rust_comments(&raw)
}

#[test]
fn the_session_window_gate_is_wired_into_the_candle_write_path() {
    let src = candle_writer_src();
    let start = src
        .find("pub fn append_row(&mut self, row: &ShadowSealRow)")
        .expect("ShadowCandleWriter::append_row is gone");
    let body = &src[start..(start + 3000).min(src.len())];

    assert!(
        body.contains("row_is_in_an_open_window"),
        "the session-window gate is GONE from the candle write path."
    );
    assert!(
        body.contains("return Ok(())"),
        "a candle window refusal must `return Ok(())` -- an Err is a loss on \
         every caller of this writer."
    );
}

#[test]
fn the_candle_gate_reads_the_bucket_and_never_the_seal_instant() {
    let src = candle_writer_src();
    let start = src
        .find("pub fn append_row(&mut self, row: &ShadowSealRow)")
        .expect("ShadowCandleWriter::append_row is gone");
    let body = &src[start..(start + 3000).min(src.len())];

    // THE trap this whole gate had to be designed around. A candle's `ts` is
    // the BUCKET-OPEN instant; the row is BUILT later, at the seal. The M1
    // bucket that opens 15:39:00 seals at watermark 15:40:02 -- two seconds
    // AFTER the window closes -- and `force_seal_all` at close plus the boot
    // recovery drain are later still. A gate that tested the seal instant, or
    // any wall clock, would discard the final bar of every session, every day,
    // and the D1 bar with it (D1 stamps 09:00, but seals last of all).
    assert!(
        body.contains("row.timestamp_ist_nanos"),
        "the candle gate must read `row.timestamp_ist_nanos` -- the BUCKET-OPEN \
         instant."
    );
    assert!(
        !body.contains("Utc::now") && !body.contains("received_at"),
        "the candle gate must not consult a wall clock or a receipt stamp. The \
         15:39 M1 bucket seals at 15:40:02, after the window closes: testing \
         when the row was BUILT discards the last bar of every session."
    );
}

#[test]
fn the_candle_refusal_counter_carries_no_feed_label() {
    let src = candle_writer_src();
    let start = src
        .find("struct CandleOutOfWindowCounters")
        .expect("CandleOutOfWindowCounters is gone");
    let block = &src[start..(start + 1500).min(src.len())];

    // Deliberate asymmetry with the tick and depth counters, pinned so it is
    // not "fixed" into a defect. ShadowCandleWriter is feed-agnostic and
    // `row.feed` varies per row, so a per-row `feed` label would be a
    // non-literal label value -- which drops `metrics::counter!` to its
    // ALLOCATING arm on the seal path. That is the record_ws_lag class of
    // defect: ~36M allocations/hour the last time it shipped, on a path this
    // repository gates with DHAT precisely to keep it allocation-free.
    assert!(
        !block.contains("\"feed\""),
        "the candle refusal counter grew a `feed` label. `row.feed` varies per \
         row here, so the label value is non-literal and the macro takes its \
         allocating arm on the seal path -- the record_ws_lag defect, 36M \
         allocations/hour."
    );
    assert!(
        block.contains("increment(0)"),
        "the candle refusal handles are no longer seeded at 0; the CloudWatch \
         agent would swallow the first increment as its delta baseline."
    );
}
