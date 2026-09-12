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
    production_str(&read(path))
}

/// `production`, over a string rather than a file.
///
/// `production` DELEGATES to this rather than duplicating its body, which is
/// the point: the self-test exercises this function, so a duplicated body
/// would let `production` rot into `read` while the self-test stayed green —
/// a scanner that no longer strips comments, proven by a copy of itself that
/// still does.
fn production_str(text: &str) -> String {
    text.lines()
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
    block_at(src, "\"unsubscribe_ignored\"").expect(
        "the ignored-unsubscribe error! lost its `source` label. Every other ghost source in \
         this family is distinguished by that field alone — `ghost_redial_exhausted`, \
         `ghost_instrument`, `depth_unsubscribe_sent` — and all of them share one ErrorCode, \
         so without it a reader (and any future filter) cannot tell which of them a line is. \
         Per the noise lock §2.3w this family is log-only today: NO CloudWatch filter matches \
         it, so `source` is the ONLY thing that makes these lines findable.",
    )
}

/// The macro call that a `source = "..."` label sits inside, from the label to
/// the closing `);`.
///
/// Anchored on the label rather than a line number, so a comment added above
/// it cannot move the window — the false-positive failure the first-packet
/// guard records having actually suffered. The window deliberately starts at
/// the label and not at the macro name, so the returned slice never reaches
/// backwards into a preceding statement.
fn block_at<'a>(src: &'a str, label: &str) -> Option<&'a str> {
    let at = src.find(label)?;
    let end = src[at..].find(");").map_or(src.len(), |rel| at + rel);
    Some(&src[at..end])
}

/// The `tracing` macro that a `source = "..."` label is emitted by.
///
/// WHY THIS EXISTS, and it is the hole that mattered most. Every other
/// assertion in this file checks a FIELD inside a line that is assumed to be
/// emitted at all. Production runs `level = "info"` (`config/base.toml`,
/// `config/production.toml`), so a one-token downgrade — `info!` to `debug!`,
/// `error!` to `trace!` — deletes the entire evidence stream in prod, is
/// invisible in review, breaks no test, and costs the trading session this
/// guard was written to protect. A `debug!` line satisfies every `contains`
/// in this file.
fn macro_before(src: &str, label: &str) -> String {
    let at = src
        .find(label)
        .unwrap_or_else(|| panic!("label {label} is gone"));
    // Walk back to the nearest macro invocation. The label is the FIRST field
    // of every line here, so the `!(` that precedes it is the emitting macro.
    let head = &src[..at];
    let bang = head.rfind("!(").unwrap_or_else(|| {
        panic!("no macro invocation precedes {label} — the log line was restructured")
    });
    let start = head[..bang]
        .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map_or(0, |i| i + 1);
    head[start..bang].to_string()
}

#[test]
fn every_ghost_line_is_emitted_at_a_level_production_actually_logs() {
    let drain = production(DRAIN);
    for label in ["\"unsubscribe_ignored\"", "\"ghost_redial_exhausted\""] {
        assert_eq!(
            macro_before(&drain, label),
            "error",
            "{label} is no longer an error!. Prod runs at `info`, so `debug!`/`trace!` would \
             make it invisible on the box while every other test in this file stays green — \
             the evidence is only collectable on a live session, so the regression costs a \
             whole trading day and is found the next morning with nothing to re-run."
        );
    }

    let supervisor = production(SUPERVISOR);
    assert_eq!(
        macro_before(&supervisor, "\"depth_unsubscribe_sent\""),
        "info",
        "the unsubscribe-sent line is no longer an info!. A downgrade to `debug!` deletes the \
         ASK half of the evidence in production — the half that has no redial ceiling and is \
         therefore the ONLY record covering the 78.5% of ghosting that happens after every \
         socket has exhausted its redials."
    );
    assert_eq!(
        macro_before(&supervisor, "\"depth_unsubscribe_timed_out\""),
        "warn",
        "the timed-out-unsubscribe line is no longer a warn!. This is the arm whose frame MAY \
         have landed and is deliberately not reverted; without its record a later ghost for \
         that instrument reads as the vendor ignoring a request we cannot show we sent."
    );
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
    // Anchored INSIDE the Ghost arm, not checked as two independent `contains`
    // over the whole file. Unanchored, the record could sit in the
    // `RecentlyDropped` or `Held` arm and both assertions would still pass —
    // and then the id on the line names a contract that did NOT ghost, which
    // is the wrong-id-in-a-vendor-ticket outcome this test's own message calls
    // worse than the gap it replaced.
    let arm = src
        .find("DepthFrameClass::Ghost => {")
        .expect("the Ghost arm is gone from the depth classify match");
    let next_arm = src[arm..]
        .find("DepthFrameClass::RecentlyDropped")
        .map(|rel| arm + rel)
        .expect("the RecentlyDropped arm is gone — the Ghost arm no longer has a sibling");
    let ghost_arm = &src[arm..next_arm];
    assert!(
        ghost_arm.contains("out.ghost_instrument = Some((header.security_id, segment))"),
        "the Ghost arm no longer records the instrument it saw. The error! would still COMPILE \
         and still print a `security_id` — the unreachable sentinel, 0 — so this regression \
         reports a contract id of zero on every ghost rather than failing loudly. That is worse \
         than the gap it replaced: a wrong id in a vendor ticket is a dismissed ticket."
    );
    assert!(
        ghost_arm.contains("out.ghost_instrument_shared = true"),
        "the Ghost arm no longer flags a SECOND distinct ghosting instrument. Without it, \
         `security_id` beside `ghost_packets` reads as \"this contract ghosted N times\" — false \
         whenever two contracts ghost in one frame, which is the expected case: a depth-20 \
         socket drops up to four contracts a minute and every one can ghost concurrently for \
         the next ten minutes."
    );
}

#[test]
fn the_ghost_line_reports_the_segment_label_the_other_two_surfaces_use() {
    // The whole value of these lines is that they PAIR. `depth_unsubscribe_sent`
    // logs `drop_this.segment.as_str()` and `market_depth.segment` is a SYMBOL
    // written from `depth_segment_label` — both strings. A raw wire byte here
    // joins to neither, and `security_id` ALONE is not unique (I-P1-11), so a
    // numeric segment leaves the pairing ambiguous in exactly the case the
    // field was added to disambiguate.
    let src = production(DRAIN);
    for label in ["\"unsubscribe_ignored\"", "\"ghost_redial_exhausted\""] {
        let block = block_at(&src, label).unwrap_or_else(|| panic!("{label} is gone"));
        assert!(
            block.contains("segment = ghost_segment"),
            "{label} no longer carries the segment"
        );
    }
    assert!(
        !src.contains("(header.security_id, header.exchange_segment_code));"),
        "the Ghost arm went back to storing the raw segment BYTE. The paired \
         `depth_unsubscribe_sent` line and the `market_depth.segment` column are both the \
         string label, so a byte here renders the same instrument two different ways across \
         the three surfaces a vendor ticket has to join."
    );
}

#[test]
fn the_ceiling_arm_names_the_contract_too() {
    // MEASURED 2026-09-11: every socket reached the redial ceiling by 10:22 IST
    // and 78.5% of the session's 5,345,436 ghost packets arrived after that.
    // Naming the instrument only on the redial arm leaves four fifths of the
    // evidence anonymous — the precise gap this whole change exists to close.
    let src = production(DRAIN);
    let block = block_at(&src, "\"ghost_redial_exhausted\"")
        .expect("the ghost-ceiling error! lost its `source` label");
    for field in ["security_id = ghost_security_id", "segment = ghost_segment"] {
        assert!(
            block.contains(field),
            "the ghost-ceiling line lost `{field}`. After the ceiling no redial is requested, \
             so this is the ONLY line naming the instrument for the rest of the session — and \
             it covers the majority of the session's ghost packets."
        );
    }
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
    // The flag assignment opens the arm; the log must follow it and must end
    // BEFORE the next arm begins. A log that drifted onto a refusal arm would
    // record a request that never reached the wire, and every ghost paired
    // against it would read as the vendor ignoring us when in fact we never
    // asked.
    //
    // Bounded by the NEXT ARM, not by a byte count. A byte window is wrong in
    // both directions and was: at 3,000 it reached past `Ok(Err(_)) => {` —
    // the refusal arm, i.e. the exact drift this test exists to catch passed —
    // while leaving so little headroom that ~29 more lines of the dated
    // comments this repo appends by convention would fail it against unchanged
    // code. The arm boundary is the real invariant and moves with the file.
    let next_arm = src[succeeded..]
        .find("Ok(Err(_)) => {")
        .map(|rel| succeeded + rel)
        .expect("the wire-refused arm is gone — the success arm no longer has a sibling");
    assert!(
        succeeded < logged && logged < next_arm,
        "the unsubscribe-sent line is not inside the success arm (flag at {succeeded}, log at \
         {logged}, next arm at {next_arm}). Either it moved onto a refusal or timeout arm — \
         recording asks that never reached the wire, which makes every ghost paired against it \
         read as vendor fault — or the arm was restructured and the pairing this evidence \
         depends on can no longer be trusted."
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

    // The level detector must read the macro that ACTUALLY emits the label,
    // not the nearest one anywhere above it. This is the assertion that makes
    // the level tests worth anything: without it a broken `macro_before` would
    // report whatever it liked and four assertions would pass on its say-so.
    assert_eq!(
        macro_before("    error!(source = \"x\", a = b);", "\"x\""),
        "error"
    );
    assert_eq!(macro_before("info!(source = \"x\");", "\"x\""), "info");
    assert_eq!(macro_before("warn!(source = \"x\");", "\"x\""), "warn");
    assert_eq!(
        macro_before("error!(a = b);\n    debug!(source = \"x\");", "\"x\""),
        "debug",
        "the level detector latched onto an EARLIER macro — it would report `error` for a line \
         that a downgrade had turned into `debug!`, which is the exact regression the level \
         tests exist to catch"
    );
}
