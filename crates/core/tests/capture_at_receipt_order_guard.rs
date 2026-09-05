//! Pins the durability invariant the entire tick chain rests on:
//! **a frame is written to the write-ahead log BEFORE it becomes visible to
//! anything downstream.**
//!
//! # Why this needs a guard at all
//!
//! `WalRingSink::accept` runs three steps in a fixed order — WAL append, byte
//! budget, ring send. The order is load-bearing, and it is the ONLY thing that
//! makes "captured" mean "survives a process kill". Invert steps 1 and 3 and
//! every layer above still looks correct: frames flow, counters increment,
//! candles seal, tests pass. The single observable difference appears when the
//! process dies — and then the frames that were in flight are simply gone, with
//! nothing anywhere reporting a loss, because from the code's point of view
//! they were accepted.
//!
//! That is the worst shape a defect can take here: invisible until the exact
//! moment the mechanism exists to protect against.
//!
//! The 2026-08-14 audit recorded this invariant as **unpinned** (plan Item 5 of
//! `active-plan-16-socket-hardening`). The re-fold path added in the same plan
//! makes it strictly more load-bearing than before: re-folding recovered frames
//! is only meaningful if the frames were durable in the first place. This test
//! is the pin.
//!
//! # What it does NOT claim
//!
//! This is a source-order assertion, not a runtime proof. It cannot show that
//! the WAL write reached the disk — only that the code attempts durability
//! before visibility. Proving the former needs a kill-test against a real
//! segment, which is a separate exercise.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("capture_at_receipt_guard: cannot canonicalize repo root")
}

/// Strip `//` line comments so prose describing the order cannot satisfy — or
/// break — an assertion about the CODE's order.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `impl FrameSink for WalRingSink`'s `accept`, comments stripped.
fn accept_body() -> String {
    let src =
        std::fs::read_to_string(repo_root().join("crates/core/src/websocket/pool_supervisor.rs"))
            .expect("pool_supervisor.rs must be readable");

    let impl_start = src
        .find("impl FrameSink for WalRingSink")
        .expect("the WalRingSink FrameSink impl must exist — if it was renamed, update this guard");
    let after_impl = &src[impl_start..];
    let fn_start = after_impl
        .find("fn accept(")
        .expect("WalRingSink::accept must exist");
    let body = &after_impl[fn_start..];

    // Bound the slice at the end of the function so a later method cannot
    // satisfy the ordering on this one's behalf.
    let end = body.find("\n    }\n").map_or(body.len(), |i| i + 6);
    strip_line_comments(&body[..end])
}

#[test]
fn wal_append_precedes_ring_send_in_capture_at_receipt() {
    let body = accept_body();

    let wal = body
        .find("append_with_seq")
        .expect("accept must append to the WAL — capture-at-receipt is the durability floor");
    let send = body
        .find("try_send")
        .expect("accept must hand the frame to the ring");

    assert!(
        wal < send,
        "CAPTURE-AT-RECEIPT ORDER INVERTED.\n\n\
         `WalRingSink::accept` makes the frame visible downstream (`try_send`) \
         BEFORE writing it to the write-ahead log (`append_with_seq`).\n\n\
         Every layer above will still look correct — frames flow, counters \
         increment, candles seal, tests pass. The difference appears only when \
         the process dies: frames in flight are gone, and nothing reports a \
         loss, because the code considered them accepted.\n\n\
         The WAL append must come FIRST. This is the invariant that makes \
         'captured' mean 'survives a kill', and it is what the boot-time \
         re-fold path depends on."
    );
}

/// # ⚠ 2026-09-05 — this test REPLACES `wal_drop_refuses_the_frame_rather_than_forwarding_it`,
/// # which asserted the OPPOSITE, and which had started passing vacuously
///
/// The retired test asserted that a failed WAL append returns before the ring,
/// with the message *"forwarding an un-WAL'd frame would silently downgrade the
/// durability floor to best-effort for exactly the frames most likely to be
/// lost."* That reasoning weighed durability against best-effort. It weighed the
/// wrong pair: the alternative to a best-effort fold is not a durable frame, it
/// is **no frame at all**. A WAL queue that is full and a database that is
/// unwritable are different failures with different causes, and refusing the
/// frame on the first treated the second as certain when it was often false.
///
/// So the behaviour is deliberately reversed: a WAL refusal now counts
/// `tv_dhan_ws_wal_dropped_total` exactly as before and then **falls through**
/// to the ring, and the terminal outcome becomes `CapturedLiveOnly` — folded
/// this session, not replayable, and logged as such rather than silently.
///
/// **The reason this test exists at all, rather than an edit, is the more
/// important half.** After the reversal the OLD assertion still PASSED. It
/// searched for the first `return FrameSinkOutcome::WalDropped` after the
/// append and compared its position against `try_send`; the reversal left three
/// such returns in the ring-refusal arms, the first of which sits above
/// `try_send` in source order. So the guard went on reporting "the frame is
/// refused before the ring" about code that forwards it — satisfied by a string
/// whose meaning had inverted underneath it. A position comparison cannot see a
/// condition, and every assertion here is written to fail on the old shape as
/// well as on a careless new one.
///
/// What this pins:
///   1. the WAL refusal is a BINDING, never an unconditional early return;
///   2. no `return FrameSinkOutcome::WalDropped` sits between the append and
///      the byte budget — that is exactly the retired early return;
///   3. every `WalDropped` return in the function is guarded by `wal_refused`,
///      so total loss is claimed only when the ring refused it too;
///   4. the success path distinguishes `CapturedLiveOnly` from `Captured`.
#[test]
fn a_wal_refusal_falls_through_to_the_ring_instead_of_dropping_the_frame() {
    let body = accept_body();
    assert!(
        wal_refusal_falls_through(&body),
        "WAL-REFUSAL EARLY RETURN IS BACK.\n\n\
         `WalRingSink::accept` refuses a frame outright because the write-ahead \
         QUEUE was full, without ever offering it to the ring. That converts a \
         durability-tier stall into a certain tick loss even when the database \
         is perfectly healthy and would have taken the frame.\n\n\
         The append must bind its outcome (`let wal_refused = ...`), fall \
         through, and claim `WalDropped` only inside a `wal_refused` check on a \
         ring-refusal arm. See this test's doc comment for why the previous \
         guard asserted the reverse and why it stopped meaning anything."
    );
}

/// The fall-through rule as a pure function, so it is provable against a KNOWN
/// BAD body — including the exact pre-2026-09-05 shape, which the retired
/// assertion accepted and this one must reject.
fn wal_refusal_falls_through(body: &str) -> bool {
    let Some(append) = body.find("append_with_seq") else {
        return false;
    };
    let after = &body[append..];

    // (1) the outcome is BOUND, not branched on inline and returned.
    if !body[..append].contains("let wal_refused =") {
        return false;
    }
    // (2) the byte budget must be reached: no `WalDropped` return may sit
    //     between the append and the reservation. That window is precisely
    //     where the retired early return lived.
    let Some(reserve) = after.find("try_reserve") else {
        return false;
    };
    if after[..reserve].contains("return FrameSinkOutcome::WalDropped") {
        return false;
    }
    // (3) every WalDropped return is conditional on the binding. Checked by
    //     requiring the two to be paired one-for-one, so an unconditional
    //     return added later cannot hide behind the guarded ones.
    let guarded = after.matches("if wal_refused {").count();
    let dropped = after.matches("return FrameSinkOutcome::WalDropped").count();
    if dropped == 0 || guarded < dropped {
        return false;
    }
    // (4) the success path names the degraded case distinctly.
    after.contains("FrameSinkOutcome::CapturedLiveOnly")
}

#[test]
fn fall_through_rule_rejects_the_retired_early_return_shape() {
    // The EXACT pre-2026-09-05 shape. The retired assertion passed it; this
    // one must not, or the reversal is unpinned and can silently regress.
    let old = r#"
        if self.spill.append_with_seq_at(self.ws_type, frame.clone(), seq) == AppendOutcome::Dropped {
            self.wal_dropped.increment(1);
            return FrameSinkOutcome::WalDropped;
        }
        match self.budget.try_reserve_detailed(len) {
            RingReserve::SlotsFull => return FrameSinkOutcome::RingFull,
            RingReserve::Granted => {}
        }
        self.ring.try_send(CapturedFrame { seq, bytes: frame })
    "#;
    assert!(
        !wal_refusal_falls_through(old),
        "the retired early-return shape MUST be rejected — it is the defect"
    );

    // Bound but returned early anyway: the same loss wearing the new syntax.
    let sneaky = r#"
        let wal_refused = self.spill.append_with_seq_at(...) == AppendOutcome::Dropped;
        if wal_refused {
            return FrameSinkOutcome::WalDropped;
        }
        match self.budget.try_reserve_detailed(len) { _ => {} }
        self.ring.try_send(f)
    "#;
    assert!(
        !wal_refusal_falls_through(sneaky),
        "binding the outcome and STILL returning before the budget is the same \
         defect with better syntax — it must be rejected"
    );

    // An unconditional WalDropped added among the guarded ones must break the
    // one-for-one pairing rather than hiding behind its neighbours.
    let unpaired = r#"
        let wal_refused = self.spill.append_with_seq_at(...) == AppendOutcome::Dropped;
        match self.budget.try_reserve_detailed(len) {
            RingReserve::BytesFull => {
                if wal_refused { return FrameSinkOutcome::WalDropped; }
                return FrameSinkOutcome::RingFull;
            }
            RingReserve::SlotsFull => { return FrameSinkOutcome::WalDropped; }
            RingReserve::Granted => {}
        }
        self.ring.try_send(f);
        FrameSinkOutcome::CapturedLiveOnly
    "#;
    assert!(
        !wal_refusal_falls_through(unpaired),
        "an UNCONDITIONAL WalDropped beside the guarded ones must be rejected"
    );

    // The real shape, in miniature — this must pass, or the rule is unusable.
    let good = r#"
        let wal_refused = self.spill.append_with_seq_at(...) == AppendOutcome::Dropped;
        if wal_refused { self.wal_dropped.increment(1); }
        match self.budget.try_reserve_detailed(len) {
            RingReserve::BytesFull => {
                if wal_refused { return FrameSinkOutcome::WalDropped; }
                return FrameSinkOutcome::RingFull;
            }
            RingReserve::Granted => {}
        }
        self.ring.try_send(f);
        if wal_refused { FrameSinkOutcome::CapturedLiveOnly } else { FrameSinkOutcome::Captured }
    "#;
    assert!(
        wal_refusal_falls_through(good),
        "the shipped shape must pass, or this rule rejects the code it describes"
    );
}

#[test]
fn byte_budget_is_reserved_before_the_send_not_after() {
    let body = accept_body();

    let reserve = body
        .find("try_reserve")
        .expect("the ring byte budget must be reserved in accept");
    let send = body.find("try_send").expect("ring send must exist");

    assert!(
        reserve < send,
        "The byte budget is reserved AFTER the send. A reservation taken after \
         a successful send cannot be refused, so the bound stops bounding — \
         and one taken for a send that then fails leaks, ratcheting the budget \
         down until it refuses everything, which reads as the feed dying for no \
         reason."
    );
}

#[test]
fn guard_is_not_vacuous() {
    // Every assertion above is an ORDER comparison, which passes trivially if
    // the scanner returns an empty body. Prove it found real code.
    let body = accept_body();
    assert!(
        body.len() > 200,
        "extracted accept() body is implausibly short ({} bytes) — the scanner \
         is broken and the ordering assertions would pass vacuously",
        body.len()
    );
    for needle in ["append_with_seq", "try_reserve", "try_send"] {
        assert!(
            body.contains(needle),
            "extracted body is missing `{needle}` — scanner broken"
        );
    }
}

/// The ordering rule as a pure function, so it can be tested against a KNOWN
/// BAD input as well as the real source.
///
/// Extracted deliberately: the assertions above compare positions in the live
/// file, and a scanner that silently stopped finding either needle would make
/// them pass forever. A guard that has never been shown to FAIL is not a guard.
fn wal_precedes_send(body: &str) -> bool {
    match (body.find("append_with_seq"), body.find("try_send")) {
        (Some(wal), Some(send)) => wal < send,
        _ => false,
    }
}

#[test]
fn ordering_rule_rejects_an_inverted_body() {
    // Correct order — WAL first.
    let good = r#"
        let seq = next_frame_seq();
        if self.spill.append_with_seq(self.ws_type, frame.clone(), seq) == AppendOutcome::Dropped {
            return FrameSinkOutcome::WalDropped;
        }
        self.ring.try_send(CapturedFrame { seq, bytes: frame })
    "#;
    assert!(wal_precedes_send(good), "the correct order must pass");

    // INVERTED — visible downstream before it is durable. This is the defect
    // the guard exists to catch, and it must be rejected.
    let bad = r#"
        let seq = next_frame_seq();
        self.ring.try_send(CapturedFrame { seq, bytes: frame.clone() });
        if self.spill.append_with_seq(self.ws_type, frame, seq) == AppendOutcome::Dropped {
            return FrameSinkOutcome::WalDropped;
        }
    "#;
    assert!(
        !wal_precedes_send(bad),
        "an inverted body MUST be rejected — otherwise this guard is decoration"
    );

    // A body missing the WAL append entirely is also a failure, not a pass.
    let missing = "self.ring.try_send(CapturedFrame { seq, bytes: frame })";
    assert!(
        !wal_precedes_send(missing),
        "a body with no WAL append at all must fail, never pass by absence"
    );
}

#[test]
fn the_real_source_satisfies_the_same_rule() {
    // Ties the pure rule back to the live file, so the two can never drift.
    assert!(
        wal_precedes_send(&accept_body()),
        "the real WalRingSink::accept must satisfy the same rule the fixtures do"
    );
}
