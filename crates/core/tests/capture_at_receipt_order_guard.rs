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

#[test]
fn wal_drop_refuses_the_frame_rather_than_forwarding_it() {
    let body = accept_body();

    // A failed WAL append must ABORT the frame, not fall through to the ring.
    // Forwarding an un-WAL'd frame would silently downgrade the durability
    // floor to best-effort for exactly the frames most likely to be lost.
    let wal_pos = body.find("append_with_seq").expect("WAL append must exist");
    let after_wal = &body[wal_pos..];
    let early_return = after_wal
        .find("return FrameSinkOutcome::WalDropped")
        .expect(
            "a failed WAL append must return `WalDropped` — a frame that could not be made \
             durable must never reach the ring",
        );
    let send_pos = after_wal
        .find("try_send")
        .expect("ring send must exist after the WAL append");

    assert!(
        early_return < send_pos,
        "A failed WAL append falls through to the ring instead of refusing the \
         frame. That silently downgrades the durability floor to best-effort \
         for precisely the frames most at risk of being lost."
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
