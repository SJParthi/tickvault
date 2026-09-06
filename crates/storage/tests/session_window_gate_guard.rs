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
//!
//! ## SCOPE BOUNDARY -- what this gate deliberately does NOT cover
//!
//! The gate is wired into the four LIVE-CAPTURE write paths: `ticks`,
//! `market_depth`, the shadow candle tables, and the spill-replay round that
//! can re-offer rows a pre-gate binary wrote. It is NOT wired into
//! `dhan_rest_1m_tape` (`dhan_live_crossverify_persistence::append_rest_tape`,
//! called once per day from the post-close cross-verification sweep), and that
//! omission is deliberate rather than an oversight.
//!
//! That table is the VENDOR's own tape, stored -- in its call site's words --
//! "BEFORE any judgement is applied to it". Its entire purpose is to preserve
//! what the exchange said so a comparison verdict can be re-derived later.
//! Filtering it through OUR window would destroy exactly the evidence it exists
//! to hold, and would do so silently: a vendor bar we refused would be
//! indistinguishable from a vendor bar that never existed. It is also a
//! different class of row -- a once-daily REST record in its own table, not a
//! live tick -- so nothing about it reaches the tick or depth loss counters.
//!
//! Stated here because a reader who finds four gated writers can reasonably
//! conclude the gate is universal. It is not, and the fifth writer is
//! ungated on purpose.

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

/// True when any line of `body` DECLARES a `static` item.
///
/// Not a substring search for `"static "`: Rust spells the `'static` lifetime
/// with the same six letters, and both of the bodies this file inspects
/// mention `&'static str` in their own signatures. The first version of this
/// check failed on that prose while the code was correct -- a guard whose
/// first act is a false positive is a guard someone deletes.
fn declares_a_static(body: &str) -> bool {
    body.lines()
        .any(|l| l.trim_start().starts_with("static ") || l.trim_start().starts_with("static\n"))
}

/// The refusal log throttle must be per-reason and per-writer, never a
/// process-wide `static`.
///
/// This is a SOURCE assertion because the shape is what matters, and the shape
/// is what regressed. Until 2026-09-06 `note` held one `static SEEN` shared by
/// every call site in the process -- two on the tick writer, three on the depth
/// writer. Depth carries a MEASURED 24x the tick row volume and refuses EVERY
/// row outside 09:00-15:39:59 IST, so an ordinary pre-open depth burst drove
/// the shared counter past 2^18 within minutes and the next permitted log sat
/// at 2^19. A rare tick refusal arriving after it -- `ts_out_of_plausible_band`,
/// the corrupt-frame signal, whose ONLY production surface is this log because
/// the counter reaches no EMF selector and no alarm -- then waited ~260,000
/// events for permission to speak.
///
/// A common benign event starving the rare real one is the throttle failing in
/// the direction a throttle exists to prevent.
#[test]
fn the_refusal_log_throttle_is_not_a_process_wide_static() {
    let src = tick_persistence_src();

    let at = src
        .find("fn throttle_tick(&self, idx: usize) -> Option<u64>")
        .expect(
            "OutOfWindowCounters::throttle_tick is gone. It is the throttle \
             DECISION, and the unit tests assert on it directly -- deleting it \
             leaves them asserting on a field a refactor can turn into an \
             unread mirror.",
        );
    // Bounded at the function's own closing brace, not a fixed character
    // count. A 400-char window overran into the doc comment on `note`, whose
    // phrase `&'static str` contains the literal this test searches for -- so
    // the guard failed on prose while the code was correct.
    let end = src[at..].find("\n    }\n").map_or(src.len(), |o| at + o);
    let body = &src[at..end];
    assert!(
        body.contains("self.seen[idx]"),
        "throttle_tick no longer reads the PER-WRITER, PER-REASON state. \
         Anything else -- a static, a shared slot, a single index -- lets one \
         writer's volume decide when another writer may log."
    );
    assert!(
        !declares_a_static(body),
        "throttle_tick declares a `static`. That is the exact regression: \
         process-wide throttle state, silenced by whichever writer is loudest."
    );

    // And `note` must route through it rather than growing a second, private
    // throttle beside the first.
    let note_at = src
        .find("pub(crate) fn note(&self, reason: &'static str) {")
        .expect("OutOfWindowCounters::note is gone");
    let note_end = src[note_at..]
        .find("\n    }\n")
        .map_or(src.len(), |o| note_at + o);
    let note_body = &src[note_at..note_end];
    assert!(
        note_body.contains("self.throttle_tick(idx)"),
        "note no longer routes through throttle_tick, so the unit tests pin a \
         function production does not call."
    );
    assert!(
        !declares_a_static(note_body),
        "note declares a `static` again -- the shared throttle is back."
    );
}

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

    // QUALIFIED with `session_window::` deliberately, 2026-09-06. The bare
    // string "row_is_in_an_open_window" is a SUBSTRING of
    // "arrival_row_is_in_an_open_window", so it matched BOTH forms and this
    // assertion could not tell the two clocks apart in either direction -- an
    // adversarial sweep bite-proved that the candle writer could be swapped to
    // the graced form with all 13 guard tests still green. The qualified form
    // cannot match the arrival name, because `session_window::arrival_row_...`
    // does not contain `session_window::row_...`.
    assert!(
        body.contains("session_window::arrival_row_is_in_an_open_window"),
        "`DepthWriter::append_row` no longer gates on the ARRIVAL clock. Depth
         has no exchange timestamp -- its ILP stamp IS the receipt instant -- so
         the strict form refuses books the vendor merely delivered late (measured
         p99 46.4s, max 198.7s) and the refusal is permanent. Depth is also 24x
         the tick row volume, so an ungated writer defeats the window rule at the
         one table where it costs the most disk."
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

    // QUALIFIED, and the arrival form is BANNED here — see the note on the
    // depth guard above. Candles carry an EXCHANGE bucket, not a receipt, so
    // admitting the arrival grace would let post-close events into candles and
    // breach the 2026-08-25/26 pre-open rule in websocket-connection-scope-lock.
    assert!(
        body.contains("session_window::row_is_in_an_open_window"),
        "the session-window gate is GONE from the candle write path."
    );
    assert!(
        !body.contains("session_window::arrival_row_is_in_an_open_window"),
        "the candle write path is using the ARRIVAL-graced predicate. Candles are
         bucketed on the EXCHANGE clock, so the grace has no meaning here and
         would admit post-close events into candles — the exact leak the
         2026-08-25 and 2026-08-26 rulings forbid."
    );
    // EVERY refusal arm returns Ok, not just one -- the same hardening the
    // DEPTH guard above already carries, applied here 2026-09-06 after an
    // adversarial sweep pointed out that this assertion had been left in the
    // weak form the depth one was fixed out of. `body.contains("return Ok(())")`
    // over a region holding TWO such returns is satisfied by whichever arm you
    // did not break. That is verbatim the defect the depth guard's own comment
    // records being caught by a bite test; the candle guard was simply never
    // given the same treatment. Consequence is bounded -- the candle gate
    // cannot fire today (the fold's own window is byte-identical and
    // `bucket_start` clamps to 09:00) -- but a guard whose weakness is known
    // and left in place is worse than one nobody has looked at.
    let oks = body.matches("return Ok(())").count();
    assert!(
        oks >= 2,
        "candle window-refusal arms must EACH `return Ok(())` -- found {oks}. \
         An Err is a loss on every caller of this writer."
    );
    assert!(
        !body.contains("return Err"),
        "a candle window refusal returns Err. The caller turns that into a \
         real loss signal; a row outside the window is a REFUSAL, not a loss."
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

// ---------------------------------------------------------------------------
// The REPLAY path. Added 2026-09-06 after an adversarial sweep found it was
// the ONE gated write path with no wiring guard at all.
//
// The gap was not subtle and not theoretical: replacing
// `let (chunk, refused_lines) = retain_lines_in_open_window(raw);` with
// `let chunk = raw;` left EVERY test in the module passing, because they all
// call the filter DIRECTLY and none of them proves the drain calls it. The
// module's own doc calls this "the one path that can still write an ungated
// row" -- so the one path that most needed a wiring pin was the one that had
// none, while the three writers each had two.
// ---------------------------------------------------------------------------

fn replay_src() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tick_spill_replay.rs");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    strip_rust_comments(&raw)
}

#[test]
fn the_session_window_gate_is_wired_into_the_spill_replay_path() {
    let src = replay_src();

    // The filter must EXIST...
    assert!(
        src.contains("fn retain_lines_in_open_window("),
        "retain_lines_in_open_window is gone -- spill replay POSTs raw ILP bytes \
         straight to QuestDB, so without it a file written by a pre-gate binary \
         re-introduces exactly the rows the three writers now refuse."
    );

    // ...and, the half that was missing, it must be CALLED FROM THE DRAIN.
    // Scoped to the POST loop rather than the whole file, so a call that only
    // appears inside `mod tests` cannot satisfy it.
    let loop_at = src
        .find("pub async fn replay_spill_dir(")
        .expect("replay_spill_dir must exist");
    let tests_at = src.find("\nmod tests {").unwrap_or(src.len());
    assert!(
        loop_at < tests_at,
        "replay_spill_dir moved below the test module; this scan is now meaningless"
    );
    let production = &src[loop_at..tests_at];

    assert!(
        production.contains("retain_lines_in_open_window(raw, is_arrival_clock)"),
        "replay_spill_dir no longer filters each chunk through \
         retain_lines_in_open_window. Every unit test in that module calls the \
         filter directly and would still pass -- this assertion is the only \
         thing standing between a deleted call and an ungated write path."
    );

    // And the clock must be CHOSEN FROM THE DIRECTORY. A `bool` parameter that
    // is always `false` would satisfy the call-shape assertion above while
    // restoring the exact loss it was added to close: the depth spill judged on
    // the exchange clock deletes books the depth WRITER accepted, and a
    // fully-refused file is truncated, so those bytes are gone for good.
    assert!(
        production.contains("DEPTH_SPILL_DIR"),
        "replay_spill_dir no longer derives the clock from the directory being
         drained. The depth spill is ARRIVAL-clocked and every other spill dir is
         exchange-clocked; without this the two disagree and replay silently
         deletes late depth rows the writer deliberately kept."
    );

    // And it must gate what is SENT, not merely compute a number. A call whose
    // result is dropped is the same defect wearing a function call.
    let call_at = production
        .find("retain_lines_in_open_window(raw, is_arrival_clock)")
        .expect("checked above");
    let after = &production[call_at..];
    let post_at = after
        .find("client.post(url)")
        .expect("replay_spill_dir no longer POSTs after filtering");
    let send_body = &after[..post_at];
    assert!(
        send_body.contains("chunk.is_empty()"),
        "the filtered chunk is no longer checked for emptiness before the POST; \
         an all-refused chunk would be sent as an empty body."
    );
    assert!(
        after[..post_at].contains("body(chunk)") || after.contains("body(chunk)"),
        "the POST no longer sends the FILTERED chunk -- filtering and then \
         posting `raw` is the defect this guard exists to catch."
    );
}

#[test]
fn the_replay_filter_judges_only_a_plausible_epoch() {
    let src = replay_src();
    let at = src
        .find("fn line_is_positively_out_of_window(")
        .expect("line_is_positively_out_of_window must exist");
    let end = src[at..]
        .find("\nfn nanos_are_a_plausible_epoch(")
        .expect("nanos_are_a_plausible_epoch must follow it");
    let body = &src[at..at + end];

    assert!(
        body.contains("if !nanos_are_a_plausible_epoch(nanos)"),
        "the plausibility band is gone from line_is_positively_out_of_window. \
         Without it a crash-torn timestamp (`... ltp=1.0 17160237`) parses as an \
         integer, reads as 1970 in nanoseconds, and a REAL captured tick is \
         DELETED because a crash cut its stamp short. The band is what makes \
         this filter fail OPEN."
    );

    // The band must be the aggregator's, not a second one invented here.
    assert!(
        src.contains("MIN_PLAUSIBLE_EXCHANGE_TS_SECS")
            && src.contains("MAX_PLAUSIBLE_EXCHANGE_TS_SECS"),
        "the replay filter no longer uses the aggregator's plausible-epoch \
         constants. Two definitions of \"this integer is a real market \
         timestamp\" in one workspace will drift, and the one that drifts \
         silently is the one that deletes data."
    );
}

/// The replay refusal counter must be SEEDED at zero before the round can
/// increment it for real.
///
/// # The defect class (2026-08-28, measured on the live box)
///
/// The CloudWatch agent computes a counter as the DELTA between consecutive
/// samples and DROPS the first sample of a series it has never seen. A counter
/// whose first increment IS the event therefore publishes nothing on the one
/// round that matters, and an ABSENT series is indistinguishable from a healthy
/// zero one. `tv_depth_rows_spilled_total` was hidden exactly this way while its
/// sibling `tv_depth_rows_dropped_total` counted 104,540 — leaving no way to
/// tell whether those rows had been rescued or lost.
///
/// This guard is ORDERED rather than merely present: the seed must appear
/// between the function signature and the first real `.increment(refused_lines)`.
/// A seed placed after the counting site would register the series only on a
/// round that had already published its first, dropped, sample.
#[test]
fn the_replay_refusal_counter_is_seeded_before_it_can_count() {
    let src = replay_src();
    const METRIC: &str = "tv_spill_replay_rows_out_of_window_refused_total";

    let fn_start = src
        .find("pub async fn replay_spill_dir(")
        .expect("replay_spill_dir must exist -- the replay round is the seeding site");
    let body = &src[fn_start..];

    let first_real_count = body
        .find(".increment(refused_lines)")
        .expect("the replay round must still COUNT refusals -- otherwise this guard is vacuous");

    let seed_region = &body[..first_real_count];
    assert!(
        seed_region.contains(METRIC) && seed_region.contains(".increment(0)"),
        "`{METRIC}` must be seeded with `.increment(0)` inside `replay_spill_dir` BEFORE the \
         first `.increment(refused_lines)`. Without the seed the CloudWatch agent drops the \
         first sample, so the very first session that refuses spilled rows publishes nothing \
         -- the `tv_depth_rows_spilled_total` failure of 2026-08-28, repeated."
    );

    // The label must be the `&'static str` helper, never a formatted value: a
    // non-literal label value drops `metrics::counter!` to its allocating arm,
    // and this runs once per replay round on the boot path.
    assert!(
        seed_region.contains("\"dir\" => dir_label(dir)"),
        "the seed must carry the same `dir_label(dir)` label as the real count, or it registers \
         a DIFFERENT series and the real one is still unseeded."
    );
}
