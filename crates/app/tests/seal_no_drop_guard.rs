//! Ratchet: a sealed candle the writer channel refuses must go to DISK, never
//! to a counter.
//!
//! **Operator directive 2026-08-19** (verbatim, typos preserved): *"i clearly
//! told you motherfucker which is neevr ever drop any ticks irrespective of
//! any wortsc ased okay?"* and, the same day: *"never dropped or dleetd dude
//! just mvoe it to db and s3 right?"*
//!
//! ## What was actually wrong
//!
//! All three seal call sites in `dhan_feed_stack.rs` read:
//!
//! ```text
//! if tx.try_send(seal).is_err() {
//!     dropped = dropped.saturating_add(1);
//! }
//! ```
//!
//! The sealed candle was counted and thrown away whenever the seal writer
//! fell behind or had not been installed. The three-tier ring → spill → DLQ
//! cascade already existed in `seal_absorption`, but only on the CONSUMER side
//! of the channel — nothing on the producer side could reach it. The
//! `global_seal_sender` doc even described the loss as the design ("the seal
//! is dropped and the producer increments `tv_seal_producer_mpsc_full_total`").
//!
//! ## Why a source scan and not only a behavioural test
//!
//! The behaviour is covered by the unit tests on `SealOverflow::escalate`,
//! which drive real spill and DLQ writers over `tempdir`s. But the defect
//! being guarded against is not "escalation computes the wrong answer" — it is
//! "somebody writes `dropped += 1` on a `try_send` failure again", which is a
//! shape, not a value. A future refactor that reintroduces the discard would
//! leave every behavioural test green, because the escalator would still work
//! perfectly while nothing called it. So the scan is the primary guard and the
//! self-test below proves the scan can actually bite.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/app -> crates -> repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Strips `//` line comments so the doc comments in this very file — and the
/// explanatory comments beside each call site, which necessarily quote the old
/// discarding shape — cannot satisfy or trip the scan.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The exact discard shape, normalised of whitespace so reformatting cannot
/// hide it.
fn squash(src: &str) -> String {
    src.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn no_seal_call_site_discards_on_a_refused_send() {
    let code = strip_line_comments(&read("crates/app/src/dhan_feed_stack.rs"));
    let squashed = squash(&code);

    // The pre-2026-08-19 shape, in both the `is_err()` and the `if let Err`
    // spellings, with the counter bump as the whole body.
    for banned in [
        "if tx.try_send(seal).is_err() { dropped = dropped.saturating_add(1); }",
        "if tx.try_send(seal).is_err() { dropped += 1; }",
    ] {
        assert!(
            !squashed.contains(banned),
            "a refused seal must be escalated to disk, never counted away. Found the \
             discarding shape: {banned}"
        );
    }
}

#[test]
fn every_try_send_of_a_seal_is_paired_with_an_escalation() {
    let code = strip_line_comments(&read("crates/app/src/dhan_feed_stack.rs"));
    let sends = code.matches("tx.try_send(seal)").count();
    let escalations = code.matches("escalate_refused_seal(").count();

    assert!(sends > 0, "the scan is vacuous if no seal send site exists");
    // One escalation per send site, plus one per no-sender-installed arm, plus
    // the function definition itself. The inequality is deliberate: pinning an
    // exact count would break on every legitimate new call site, while
    // "at least as many escalations as sends" is the property that matters.
    assert!(
        escalations > sends,
        "every seal send site needs an escalation path (found {sends} sends, \
         {escalations} escalations)"
    );
}

#[test]
fn the_no_sender_arm_escalates_rather_than_discarding() {
    let code = strip_line_comments(&read("crates/app/src/dhan_feed_stack.rs"));
    let squashed = squash(&code);
    assert!(
        !squashed.contains(
            "let Some(tx) = sender else { dropped = dropped.saturating_add(1); return; }"
        ),
        "a missing seal writer must still route the candle to disk — a boot-order \
         problem should cost a disk write, not a day of candles"
    );
}

#[test]
fn boot_installs_the_overflow_escalator_beside_the_sender() {
    let main = strip_line_comments(&read("crates/app/src/main.rs"));
    assert!(
        main.contains("set_global_seal_sender("),
        "the boot sender install must exist for this pairing to mean anything"
    );
    assert!(
        main.contains("set_global_seal_overflow("),
        "boot installs the sender but NOT the durable tier — that is exactly the \
         pre-2026-08-19 behaviour, where a full or absent channel discarded the seal"
    );
}

#[test]
fn the_durable_tiers_are_shared_not_duplicated() {
    let absorption = strip_line_comments(&read("crates/storage/src/seal_absorption.rs"));
    // Sharing the INSTANCE matters, not just the directory: two independent
    // spill writers on one day-file each cache their own append handle behind
    // their own mutex, and interleave partial writes.
    assert!(
        absorption.contains("spill: Arc<SealSpillWriter>"),
        "the spill writer must be shareable so the producer-side escalator appends \
         through the SAME mutex the pipeline uses"
    );
    assert!(
        absorption.contains("pub fn spill_handle(&self)"),
        "the pipeline must expose its spill writer for the escalator to share"
    );
    assert!(
        absorption.contains("pub fn dlq_handle(&self)"),
        "the pipeline must expose its DLQ writer for the escalator to share"
    );
}

#[test]
fn rescued_is_not_folded_into_dropped() {
    let code = strip_line_comments(&read("crates/app/src/dhan_feed_stack.rs"));
    assert!(
        code.contains("tv_dhan_feed_seals_rescued_total"),
        "rescued seals need their own counter name"
    );
    // A rescued seal is on disk awaiting the boot drain. Counting it as a loss
    // would make the AGGREGATOR-DROP-01 loss alarm fire for a working rescue,
    // which trains the operator to ignore the one alarm that means real loss.
    assert!(
        !code.contains("seals_dropped.increment(rescued)"),
        "a rescued seal is not a lost seal — it must never increment the loss counter"
    );
}

#[test]
fn guard_self_test_detects_the_reintroduced_discard() {
    // Proves the scan bites in BOTH directions: the exact shape this guard
    // bans must be detected when present, and comment text must not trip it.
    let reintroduced =
        "if tx.try_send(seal).is_err() {\n    dropped = dropped.saturating_add(1);\n}";
    assert!(
        squash(&strip_line_comments(reintroduced))
            .contains("if tx.try_send(seal).is_err() { dropped = dropped.saturating_add(1); }"),
        "the scan must detect the discarding shape when it is real code"
    );

    let only_a_comment =
        "// if tx.try_send(seal).is_err() { dropped = dropped.saturating_add(1); }";
    assert!(
        !squash(&strip_line_comments(only_a_comment))
            .contains("if tx.try_send(seal).is_err() { dropped = dropped.saturating_add(1); }"),
        "a comment describing the old shape must not trip the scan — this file and the \
         call sites both quote it deliberately"
    );
}

/// Name-matched coverage for `escalate_refused_seal`, the free function that
/// IS the no-drop policy.
///
/// It is exercised behaviourally through the source scans above and through
/// the `SealOverflow` unit tests in `tickvault-storage`; this asserts the one
/// property that is only visible from OUTSIDE those tests — the honest
/// answer when no durable tier was ever installed.
#[test]
fn escalate_refused_seal_reports_lost_when_no_durable_tier_is_installed() {
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

    // This test binary never installs a global escalator, so the accessor
    // returns `None`. Claiming "rescued" there would be exactly the false-OK
    // the policy exists to prevent: the operator would read a healthy rescue
    // count while candles evaporated.
    let mut state = LiveCandleState::empty();
    state.bucket_start_ist_secs = 34_200;
    state.close = 101.0;
    let seal = BufferedSeal::new(13, 0, TfIndex::M1, state, Feed::Dhan);

    assert_eq!(
        tickvault_app::dhan_feed_stack::escalate_refused_seal(&seal),
        tickvault_app::dhan_feed_stack::SealRefusal::Lost,
        "with no durable tier installed the only honest answer is Lost"
    );
}

/// Name-matched coverage for the `seals_rescued` accessor: it must be READ
/// somewhere, not merely incremented. A counter with no read-out is the
/// failure mode this lane has already shipped twice.
#[test]
fn seals_rescued_is_reported_beside_seals_dropped() {
    let code = std::fs::read_to_string(repo_root().join("crates/app/src/dhan_feed_stack.rs"))
        .expect("read dhan_feed_stack.rs");
    assert!(
        code.contains("seals_rescued = ingest.seals_rescued()"),
        "the rescued count must appear in the drain summary — a non-zero `rescued` \
         beside a zero `dropped` is the policy working, and that is only readable \
         when the pair is logged together"
    );
}
