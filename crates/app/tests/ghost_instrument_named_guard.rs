//! A ghost must be NAMEABLE (2026-09-11).
//!
//! # What this exists to prevent
//!
//! Two full trading sessions proved that Dhan ignores the depth unsubscribe:
//! code **25** on 2026-09-10 and code **24** on 2026-09-11, each producing an
//! identical failure signature — 80 `unsubscribe_ignored` lines, split 40/40
//! across depth-20 and depth-200, on all ten depth sockets, every socket
//! reaching `GHOST_REDIAL_SESSION_CEILING`. Across both sessions this process
//! **could not say which contract ghosted.**
//!
//! The reason is narrow and was entirely ours. Two log sites decide it, and
//! both dropped the instrument on the floor:
//!
//! | Site | What it logged before | What was lost |
//! |---|---|---|
//! | the unsubscribe SUCCESS arm (`pool_supervisor.rs`) | **nothing at all** — only the REFUSAL arm named an instrument | which contract we asked to drop, and when |
//! | the ghost `error!` (`dhan_feed_stack.rs`) | connection, endpoint, packet count, redials | which contract was still arriving |
//!
//! So every one of the 160 ignored unsubscribes took the silent path. The
//! operator's own Dhan-support workflow requires *"precise contract labels …
//! SecurityId for every contract cited"*, and that requirement could not be
//! met from our own telemetry — on the one failure a vendor ticket exists to
//! report.
//!
//! It also leaves a diagnostic question unanswerable: whether ONE contract
//! survived eight redials, or EIGHT different contracts were each newly
//! ignored. Those have opposite causes, and no counter distinguishes them.
//!
//! # Why a guard rather than trust
//!
//! Both fields are pure observability. Nothing breaks if they are deleted, no
//! test fails, no counter moves, and the lane keeps running — which is exactly
//! the shape this repository has watched decay before. The evidence they carry
//! is only collectable on a live trading session, so a silent regression costs
//! a whole session and is discovered a day later, with nothing to re-run.

use std::fs;

const SUPERVISOR: &str = "../core/src/websocket/pool_supervisor.rs";
const DRAIN: &str = "src/dhan_feed_stack.rs";

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// `read`, with every `//` line comment removed.
///
/// The sibling-guard discipline, and it bites in BOTH directions here: a
/// deleted field left behind as `// security_id = ...` would keep every
/// `contains` below green while the log carries nothing, and this file's own
/// module header names `security_id` repeatedly — so a raw scan would pass on
/// the prose alone even with both call sites gone.
///
/// Line comments only. A `/* */` block inside a string literal cannot be
/// stripped without a Rust lexer; none of the scanned text uses one, and the
/// limit is recorded rather than assumed away.
fn production(path: &str) -> String {
    read(path)
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of the one `error!` that reports an ignored unsubscribe.
///
/// Anchored on the `source` value rather than on a line number, so a comment
/// added above it cannot move the window — the false-positive failure the
/// first-packet guard records having actually suffered.
fn unsubscribe_ignored_block(src: &str) -> &str {
    let at = src.find("\"unsubscribe_ignored\"").expect(
        "the ignored-unsubscribe error! lost its `source` label — the one CloudWatch \
                 filter that finds these lines matches on it, so without it the evidence is \
                 unreachable even when it is written",
    );
    let end = src[at..].find(");").map_or(src.len(), |rel| at + rel);
    &src[at..end]
}

#[test]
fn the_ignored_unsubscribe_error_names_the_contract() {
    let src = production(DRAIN);
    let block = unsubscribe_ignored_block(&src);
    assert!(
        block.contains("security_id = ghost_security_id"),
        "the ignored-unsubscribe error! no longer carries the ghosting instrument. This is \
         the field a Dhan support ticket needs: without it the line says a socket is \
         delivering SOMETHING it was told to drop, and the vendor can dismiss it."
    );
    assert!(
        block.contains("segment = ghost_segment"),
        "the ignored-unsubscribe error! no longer carries the segment. `security_id` ALONE \
         is not unique (I-P1-11) — a bare id names two instruments across segments, which is \
         precisely the ambiguity a vendor ticket cannot afford."
    );
}

#[test]
fn the_ghost_arm_records_the_instrument_so_the_field_is_not_a_sentinel() {
    let src = production(DRAIN);
    assert!(
        src.contains("ghost_instrument")
            && src.contains(".get_or_insert((header.security_id, header.exchange_segment_code))"),
        "the Ghost arm no longer records the instrument it saw. The error! above would still \
         COMPILE and still print a `security_id` — the unreachable sentinel, 0 — so this \
         regression reports a contract id of zero on every ghost rather than failing loudly. \
         That is worse than the gap it replaced: a wrong id in a vendor ticket is a dismissed \
         ticket."
    );
}

#[test]
fn the_successful_unsubscribe_is_logged_with_its_instrument_and_code() {
    let src = production(SUPERVISOR);
    assert!(
        src.contains("\"depth_unsubscribe_sent\""),
        "the unsubscribe SUCCESS arm stopped logging. It logged NOTHING until 2026-09-11, \
         which is why 160 ignored unsubscribes across two sessions left no record of what we \
         had asked to drop or when; only the REFUSAL arm named an instrument, and a refusal \
         is the case that did not happen."
    );
    for field in [
        "security_id = drop_this.security_id",
        "segment = drop_this.segment.as_str()",
        "request_code = FEED_UNSUBSCRIBE_TWENTY_DEPTH",
    ] {
        assert!(
            src.contains(field),
            "the unsubscribe-sent line lost `{field}`. All three are load-bearing: the id and \
             segment pair this line against the depth rows that keep arriving afterwards, and \
             the request code records WHICH value the binary sent — 25 and 24 have both now \
             shipped, and a session's evidence is worthless if the reader has to guess which \
             one produced it."
        );
    }
}

#[test]
fn the_unsubscribe_log_sits_on_the_success_arm_and_not_on_a_refusal() {
    let src = production(SUPERVISOR);
    let logged = src
        .find("\"depth_unsubscribe_sent\"")
        .expect("the unsubscribe-sent line is gone");
    let succeeded = src
        .find("unsubscribe_succeeded = true")
        .expect("the unsubscribe success flag is gone");
    // The flag assignment opens the arm; the log follows it inside the same
    // block. A log that drifted onto a refusal arm would record a request that
    // never reached the wire, and every ghost paired against it would read as
    // the vendor ignoring us when in fact we never asked.
    assert!(
        succeeded < logged && logged - succeeded < 3_000,
        "the unsubscribe-sent line is no longer adjacent to the success flag (flag at \
         {succeeded}, log at {logged}). Either it moved onto a refusal arm — recording asks \
         that never reached the wire — or the arm was restructured and the pairing this \
         evidence depends on can no longer be trusted."
    );
}

/// Bite-proof: the scanner can actually fail.
///
/// Every assertion above is a `contains` over a file that currently satisfies
/// it, so all four pass whether the scanner works or is broken. This one feeds
/// the real extractors text that must NOT satisfy them.
#[test]
fn guard_self_test() {
    // Comment stripping: the commented-out field must not count.
    let commented = production_str("    // security_id = ghost_security_id,\n    foo();");
    assert!(
        !commented.contains("security_id = ghost_security_id"),
        "the comment stripper is broken — a deleted field left behind as a comment would keep \
         every assertion in this file green while the log carries nothing"
    );

    // Block extraction: fields BELOW the error! must not be mistaken for its own.
    let src = "let x = 1;\nerror!(source = \"unsubscribe_ignored\", a = b);\n\
               info!(security_id = ghost_security_id);";
    let block = unsubscribe_ignored_block(src);
    assert!(
        !block.contains("security_id = ghost_security_id"),
        "the block extractor runs past the end of the error! — a field on a LATER statement \
         would satisfy the assertions while the ignored-unsubscribe line itself stayed \
         anonymous"
    );

    // And it must still find a field that IS inside the call.
    let real = "error!(source = \"unsubscribe_ignored\", security_id = ghost_security_id);";
    assert!(
        unsubscribe_ignored_block(real).contains("security_id = ghost_security_id"),
        "the block extractor stops too early and would fail against correct code — a guard \
         whose first act is a false positive is a guard somebody deletes"
    );
}

/// `production`, over a string rather than a file, for the self-test.
fn production_str(text: &str) -> String {
    text.lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
