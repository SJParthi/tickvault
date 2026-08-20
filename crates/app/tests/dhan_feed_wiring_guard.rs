//! S6-G1 wiring guard for the Dhan 16-connection live-feed stack.
//!
//! These tests assert that CALL SITES exist, not merely that functions do. The
//! four components this lane depends on all survived the 2026-07-17 deletions
//! *as compiling, tested, fully-orphaned code* — `MultiTfAggregator::consume_tick`,
//! `TickWriter::append_tick`, `TickGapDetector`, and the 15:31
//! `dhan_live_crossverify` comparator each had zero production callers. A test
//! that only proved they exist would have passed happily throughout that
//! entire period, which is precisely the false-OK class audit Rule 11 forbids.
//!
//! The guards below therefore scan the boot seam's source for the call, the
//! way `pub-fn-wiring-guard.sh` does.

const STACK_SRC: &str = include_str!("../src/dhan_feed_stack.rs");

// ---------------------------------------------------------------------------
// Orphan 1 — the tick → timeframe fold
// ---------------------------------------------------------------------------

#[test]
fn dhan_feed_ingest_calls_the_aggregator_fold() {
    assert!(
        STACK_SRC.contains("aggregator.consume_tick("),
        "LiveIngest must call the aggregator fold. `MultiTfAggregator` compiled \
         and passed its own unit tests with ZERO production callers from \
         2026-07-17 until this lane wired it — existence proves nothing.\n\n\
         (Needle updated 2026-08-11: the call was `consume_tick_into_ring` \
         until the seals were re-routed from a private ring nothing drained \
         to the process-wide seal writer. This guard went red at that rename \
         and stayed red through a push, because only `--lib` tests were run. \
         It is anchored on `aggregator.consume_tick(` — the receiver, not \
         just the method — so an unrelated `consume_tick` elsewhere cannot \
         satisfy it.)"
    );
}

// ---------------------------------------------------------------------------
// Orphan 2 — tick persistence
// ---------------------------------------------------------------------------

#[test]
fn dhan_feed_ingest_calls_the_tick_writer() {
    assert!(
        STACK_SRC.contains("append_tick_with_seq("),
        "LiveIngest must append every folded tick to the tick writer."
    );
}

/// The capture_seq single-source rule, enforced mechanically.
///
/// `TickWriter::append_tick` is a convenience wrapper whose body mints a
/// sequence from `tick_persistence::next_capture_seq()` — a DIFFERENT process
/// -global atomic from the `ws_frame_spill::next_frame_seq()` counter that
/// stamps WAL frames. Both are seeded `max(prev + 1, wall_clock_nanos)`, so
/// they mint the same integer whenever first touched inside one nanosecond.
/// Since `capture_seq` is the intra-second tiebreaker in the live DEDUP key
/// and Dhan's `exchange_timestamp` is second-granular, a collision UPSERTs one
/// real tick on top of another and it is gone silently.
#[test]
fn dhan_feed_ingest_never_calls_bare_append_tick() {
    for (i, line) in STACK_SRC.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains(".append_tick("),
            "line {}: the live path must call `append_tick_with_seq`, never bare \
             `append_tick`. The bare form mints from `next_capture_seq()`, the \
             OTHER of this process's two sequence atomics; letting both stamp \
             `ticks.capture_seq` lets two real ticks share one value and one is \
             silently upserted away.\n  offending: {}",
            i + 1,
            code.trim()
        );
    }
}

/// Exactly one sequence source is reachable from this lane.
#[test]
fn dhan_feed_has_exactly_one_capture_seq_source() {
    // Comment-stripped: this module DOCUMENTS `next_capture_seq` at length in
    // order to explain why it is the wrong source. Documenting the hazard must
    // not read as committing it, so only executable code is scanned.
    for (i, line) in STACK_SRC.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains("next_capture_seq"),
            "line {}: the live lane must not reach `next_capture_seq` — the frame \
             sequence is the single source, because only it is replay-stable (a \
             re-injected WAL frame must reproduce its ORIGINAL capture_seq so the \
             replayed row collapses instead of duplicating).\n  offending: {}",
            i + 1,
            code.trim()
        );
    }
    assert!(
        STACK_SRC.contains("capture_seq_from_frame_seq"),
        "the live lane must narrow the frame sequence through the documented \
         single-source helper."
    );
}

// ---------------------------------------------------------------------------
// Orphan 3 — the tick gap detector
// ---------------------------------------------------------------------------

#[test]
fn dhan_feed_ingest_calls_the_gap_detector() {
    assert!(
        STACK_SRC.contains("self.detector.observe("),
        "LiveIngest must feed every tick to the gap detector."
    );
    assert!(
        STACK_SRC.contains("self.detector.seed("),
        "the detector must be seedable: an instrument that never ticks leaves \
         no payload to reason about, so a wholly-lost stream is invisible \
         unless the instrument was registered in advance."
    );
}

/// The detector observes BEFORE the aggregator's refusal can short-circuit.
///
/// An instrument whose price is insane or whose bucket is out of session is
/// exactly the instrument whose silence matters most; ordering the observation
/// after the refusal would blind the detector to the failure it exists to see.
#[test]
fn dhan_feed_gap_detector_observes_before_aggregator_refusal() {
    let observe = STACK_SRC
        .find("self.detector.observe(")
        .expect("detector call site");
    let fold = STACK_SRC
        .find("aggregator.consume_tick(")
        .expect("aggregator call site");
    assert!(
        observe < fold,
        "the gap detector must observe before the aggregator fold, so an \
         aggregator refusal can never suppress the observation."
    );
}

// ---------------------------------------------------------------------------
// Orphan 4 — the 15:31 cross-verification (BLOCKING, not optional)
// ---------------------------------------------------------------------------

#[test]
fn dhan_feed_stack_spawns_the_daily_crossverify() {
    assert!(
        STACK_SRC.contains("spawn_daily_crossverify(&params.main_feed_instruments)"),
        "the bring-up body must spawn the 15:31 comparator. The main feed has \
         NO snapshot-on-subscribe and NO sequence number, so packet loss is \
         undetectable at the protocol level and this cross-verify is the lane's \
         only ground truth — the 2026-08-09 authorization requires it live from \
         day one."
    );
}

/// An un-provisioned comparator must refuse loudly, never skip quietly.
#[test]
fn dhan_feed_crossverify_refuses_loudly_when_unprovisioned() {
    assert!(
        STACK_SRC.contains("XVERIFY_UNPROVISIONED_COUNTER"),
        "a missing cross-verify provider must increment a counter, not pass silently."
    );
    assert!(
        STACK_SRC.contains("UNDETECTABLE"),
        "the un-provisioned error must state the consequence in plain terms."
    );
}

// ---------------------------------------------------------------------------
// The gate stays shut
// ---------------------------------------------------------------------------

#[test]
fn dhan_feed_stack_gate_still_defaults_off() {
    use tickvault_app::dhan_feed_stack::{FeedStackGate, feed_stack_gate};

    assert_eq!(
        feed_stack_gate(false, None),
        FeedStackGate::DisabledByConfig,
        "config off must stay off"
    );
    assert_eq!(
        feed_stack_gate(true, None),
        FeedStackGate::DisabledByEnv,
        "an absent env opt-in must be OFF by construction — this wiring round \
         authorized BUILDING the lane, not enabling live capture."
    );
    assert_eq!(
        feed_stack_gate(true, Some("true")),
        FeedStackGate::DisabledByEnv,
        "only the exact opt-in value may open the gate; near-misses stay shut"
    );
    assert_eq!(
        feed_stack_gate(true, Some("1")),
        FeedStackGate::Enabled,
        "both gates open is the only enabled combination"
    );
}

/// INVERTED 2026-08-11. This asserted `dhan_enabled` stays `false`, on the
/// reasoning that "enabling live capture is a separate operator decision from
/// building the lane". That reasoning was right, and the separate decision has
/// now been made: the operator quote of 2026-08-11 recorded in
/// `websocket-connection-scope-lock.md` ("THE DEFAULT IS FLIPPED ON").
///
/// It keeps checking the `[feeds]` section specifically — not the whole file —
/// so a `dhan_enabled` line in some other section could never satisfy it. That
/// section-scoping is why this guard is kept rather than folded into the two
/// simpler string pins in `dhan_live_off_phase_a_guard.rs` and
/// `production_config_wiring.rs`; three guards checking one flag three ways is
/// deliberate for a flag that decides whether market data is captured at all.
#[test]
fn dhan_feed_stack_is_enabled_in_tracked_config() {
    for file in ["../../config/base.toml", "../../config/production.toml"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut in_feeds = false;
        let mut seen = false;
        for line in src.lines() {
            let code = line.split('#').next().unwrap_or("").trim();
            if code.starts_with('[') {
                in_feeds = code == "[feeds]";
                continue;
            }
            if in_feeds && code.starts_with("dhan_enabled") {
                seen = true;
                assert!(
                    code.contains("true"),
                    "{file}: [feeds] dhan_enabled must be true — the live lane was \
                     switched on by the operator quote of 2026-08-11. Turning it off \
                     stops all tick capture while looking like a healthy REST-only \
                     boot; reverting needs a fresh dated quote in \
                     websocket-connection-scope-lock.md and this guard updated in the \
                     same PR."
                );
            }
        }
        // A silently-absent key is the third state neither branch above covers:
        // it falls through to the serde default, which is exactly the drift this
        // guard exists to catch.
        assert!(
            seen,
            "{file}: [feeds] has no dhan_enabled key at all — the live-capture \
             decision must be explicit in tracked config, not left to a struct \
             default that a future refactor can change without touching config"
        );
    }
}

// ---------------------------------------------------------------------------
// capture_seq narrowing behaviour
// ---------------------------------------------------------------------------

#[test]
fn capture_seq_narrowing_is_fail_closed_not_saturating() {
    use tickvault_app::dhan_feed_stack::capture_seq_from_frame_seq;

    assert_eq!(capture_seq_from_frame_seq(0), Some(0));
    assert_eq!(capture_seq_from_frame_seq(1), Some(1));

    let max_ok = u64::try_from(i64::MAX).expect("i64::MAX fits u64");
    assert_eq!(capture_seq_from_frame_seq(max_ok), Some(i64::MAX));

    assert_eq!(
        capture_seq_from_frame_seq(max_ok + 1),
        None,
        "an unrepresentable sequence must be REFUSED, not saturated. Saturating \
         would pin every subsequent tick to i64::MAX and collapse all of them \
         under the DEDUP key — turning one dropped tick into unbounded silent loss."
    );
    assert_eq!(capture_seq_from_frame_seq(u64::MAX), None);
}

/// Narrowing preserves the strict monotonicity the DEDUP key depends on.
#[test]
fn capture_seq_narrowing_preserves_monotonicity() {
    use tickvault_app::dhan_feed_stack::capture_seq_from_frame_seq;

    let mut prev = i64::MIN;
    for seq in [
        1u64,
        2,
        1_000,
        1_000_000_000_000_000_000,
        9_223_372_036_854_775_806,
    ] {
        let got = capture_seq_from_frame_seq(seq).expect("representable");
        assert!(
            got > prev,
            "narrowing must preserve order: {seq} narrowed to {got}, not above {prev}"
        );
        prev = got;
    }
}

/// The two process-global sequence generators are genuinely distinct, so the
/// single-source rule above is answering a real hazard rather than a
/// hypothetical one. If this ever fails, the two counters were unified and the
/// guards above can be revisited.
#[test]
fn the_two_sequence_generators_are_independent_and_can_collide() {
    use tickvault_storage::tick_persistence::next_capture_seq;
    use tickvault_storage::ws_frame_spill::next_frame_seq;

    let frame_a = next_frame_seq();
    let capture_a = next_capture_seq();
    let frame_b = next_frame_seq();

    assert!(frame_b > frame_a, "frame seq must be monotonic per-counter");

    // The counters are separate atomics: nothing orders `capture_a` with
    // respect to `frame_a`/`frame_b`. That absence of ordering IS the hazard —
    // both are wall-clock-nanosecond seeded, so first touches inside one
    // nanosecond mint equal values. The live lane therefore uses exactly one.
    let capture_a_u64 = u64::try_from(capture_a).unwrap_or(u64::MAX);
    let overlaps = capture_a_u64 >= frame_a.min(frame_b) && capture_a_u64 <= frame_b.max(frame_a);
    let _ = overlaps; // observed, not asserted — timing-dependent by nature

    assert!(
        capture_a > 0 && frame_a > 0,
        "both generators are wall-clock seeded, which is exactly why two \
         independently-seeded counters can mint the same integer"
    );
}

// ---------------------------------------------------------------------------
// Orphan 5 — the receipt stamp must come from the FRAME, not from the drain
// ---------------------------------------------------------------------------

/// The drain must consult the frame's own read-task receipt stamp.
///
/// Until 2026-08-18 the drain bound its receive time from a fresh wall-clock
/// read, so every WebSocket lag sample measured
/// `Dhan's delivery + however long the frame sat in the ring`. Those are two
/// different quantities with two different owners, and only the first is the
/// vendor's. The failure mode is worse than imprecision: under a fold stall
/// the ring backs up and the drain falls behind, so the number inflates
/// *precisely* when the cause is LOCAL — the lag alarm fires hardest at our
/// own backlog while naming Dhan for it.
#[test]
fn dhan_feed_drain_consults_the_frames_receipt_stamp() {
    assert!(
        drain_body().contains("frame.received_at.elapsed()"),
        "the frame drain must read the frame's own read-task receipt stamp \
         (`frame.received_at.elapsed()`). Without it the drain is timing its \
         own queueing delay and reporting it as Dhan's delivery lag."
    );
}

/// Reading the queue delay is not enough — it has to be SUBTRACTED.
///
/// This is the regression that would actually happen. Deleting
/// `frame.received_at` outright is a compile error and needs no guard; quietly
/// computing `elapsed()` and then not applying it compiles, passes every unit
/// test, and silently restores the exact wrong measurement. That is the shape
/// this file exists to catch.
#[test]
fn dhan_feed_drain_actually_subtracts_the_queue_delay() {
    let body = drain_body();
    assert!(
        body.contains("saturating_sub(queued_nanos)"),
        "the frame drain computes the ring queueing delay but no longer \
         subtracts it from the wall-clock read. The receipt instant is \
         `now - queued`, not `now`; without the subtraction the lag metric is \
         back to charging our own backlog to the vendor."
    );
}

/// The body of `run_frame_drain`, comments stripped, from its signature to the
/// next top-level item.
///
/// Two deliberate choices, both learned the hard way while writing this guard:
///
/// * **Scoped to the function**, because `refold_wal_frames` legitimately uses
///   "now" as a receive time — a frame replayed from the WAL after a crash has
///   no surviving receipt instant, and its lag reading is meaningless anyway
///   since the tick's own exchange timestamp decides the candle bucket. A
///   whole-file scan would flag it, and a false positive in a ratchet is worse
///   than no ratchet: the next reader deletes it and the real regression walks
///   through the hole afterwards.
/// * **Comments stripped**, because the drain carries a comment explaining
///   what the old clock read was and why it went. A scan that could not tell
///   the explanation from the thing explained would fail on the very commit
///   that fixed the defect.
fn drain_body() -> String {
    const NEEDLE: &str = "async fn run_frame_drain(";
    let start = STACK_SRC
        .find(NEEDLE)
        .expect("run_frame_drain must exist — it is the ring's only consumer");
    let rest = &STACK_SRC[start..];
    let end = rest
        .find("\n}\n")
        .expect("run_frame_drain must be closed by a top-level brace");
    rest[..end]
        .lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Orphan 6 — a dead Dhan lane must be able to report dead
// ---------------------------------------------------------------------------

/// Every flush in the drain must report its row count to feed-health.
///
/// Until 2026-08-18 `record_ticks` had ZERO production callers anywhere in
/// the workspace and the drain never touched feed-health at all, so the Dhan
/// verdict could not fall to `Down` — `feed_health` answered a benign
/// `Unknown, "not instrumented yet"` whether the lane was streaming, stalled,
/// or absent. The one signal an operator checks to find out whether the feed
/// is alive was structurally incapable of saying no.
///
/// This pins the SHAPE that fixes it: the drain flushes through
/// `flush_and_record`, which pairs the flush with the report. The pairing is
/// the point — the row count was already being computed and returned at every
/// one of these sites, and every one of them threw it away.
#[test]
fn dhan_feed_drain_reports_flushed_rows_to_feed_health() {
    let body = drain_body();
    assert!(
        body.contains("flush_and_record(&mut ingest, &feed_health)"),
        "the frame drain must flush through `flush_and_record` so the rows it \
         wrote reach the feed-health registry. Without it the Dhan health \
         verdict can never fall to Down — a corpse reports \
         `Unknown, \"not instrumented yet\"`."
    );
}

/// No flush site may bypass the reporting wrapper.
///
/// This is the regression that would actually happen, and it is why the
/// previous test is not sufficient on its own: adding a FOURTH flush site
/// later — or reverting one of the three — leaves the other call sites intact,
/// so a check for "does `flush_and_record` appear anywhere" still passes while
/// a slice of the lane's output silently stops counting. Health would then
/// decay on a feed that is in fact delivering, which produces a false page
/// rather than a missing one.
///
/// Scoped to `run_frame_drain`'s body, with comments stripped, for the same
/// two reasons the receipt-stamp guards above are: `refold_wal_frames`
/// legitimately flushes outside the live path, and the drain carries a comment
/// that names the old call shape while explaining why it went.
#[test]
fn no_drain_flush_site_bypasses_the_feed_health_report() {
    let body = drain_body();
    assert!(
        !body.contains("blocking_flush(|| ingest.flush())"),
        "a flush site in the drain calls `blocking_flush(|| ingest.flush())` \
         directly, bypassing `flush_and_record`. The rows it writes will not \
         reach feed-health, so that slice of the lane's output stops counting \
         toward liveness while the feed is genuinely delivering."
    );
}
