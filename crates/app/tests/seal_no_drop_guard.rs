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

/// A lost seal must PAGE, not just increment a counter.
///
/// This is the hole the same-day hostile audit found in the no-drop fix
/// itself. `escalate_refused_seal` returned `SealRefusal::Lost` with no log at
/// all, so the operator's only signal was `tv_dhan_feed_seals_dropped_total` —
/// which the Dhan noise lock records as *visible but unpageable*.
///
/// The case that makes it fatal: if `SealWriterRunner::new` fails at boot,
/// `main.rs` installs NEITHER the sender NOR the overflow, so every seal takes
/// the no-tier path for the life of the process. The alarmed drain counter
/// lives inside the writer loop that never spawned, so it reads a flat,
/// healthy zero all day while a whole session of candles evaporates.
#[test]
fn every_lost_seal_path_fires_aggregator_drop_01() {
    let code = strip_line_comments(&read("crates/app/src/dhan_feed_stack.rs"));

    // Both Lost arms must record. Two call sites: no-tier, and both-tiers-failed.
    assert_eq!(
        code.matches("seal_loss_alarm::record_lost_seal(").count(),
        2,
        "both Lost arms — no durable tier installed, and both disk tiers refused — \
         must record and page; a bare `return SealRefusal::Lost` is the false-OK \
         this whole policy exists to prevent"
    );
    assert!(
        code.contains("SealLossReason::NoDurableTier"),
        "the no-durable-tier arm must be distinguishable — its operator action is a \
         restart, not a disk fix"
    );
    assert!(
        code.contains("SealLossReason::BothDiskTiersFailed"),
        "the both-tiers-failed arm must be distinguishable"
    );

    let alarm = strip_line_comments(&read("crates/app/src/seal_loss_alarm.rs"));
    assert!(
        alarm.contains("ErrorCode::AggregatorDrop01"),
        "a lost seal must carry the AGGREGATOR-DROP-01 code — that is what the \
         error-code metric filter alarms on"
    );
    // The counter must be exact even though the log is throttled: at ~4,565
    // instruments across 13 emitted timeframes the no-tier case fires on every
    // seal, and an unthrottled log would bury the page and fill the disk.
    assert!(
        alarm.contains("is_power_of_two()"),
        "the log must be throttled (powers of two, the indicator::engine idiom)"
    );
    assert!(
        alarm.contains("seals_lost_total = total"),
        "a throttled line must carry the running total, or it implies a single \
         event when thousands were lost"
    );
}

// ---------------------------------------------------------------------------
// The escalation offload (2026-08-28)
//
// The escalation itself was already correct — a refused seal reached disk
// instead of a counter. What it did NOT do was reach disk somewhere other
// than the frame-drain task. All three `escalate_refused_seal` call sites run
// on the drain (the per-tick fold, the 5-second catch-up sweep, and the close
// force-seal), so every escalation charged that thread a spill-writer mutex
// plus a `write(2)` — and on spill failure a `create_dir_all`, an `open`, a
// `serde_json::to_string` HEAP ALLOCATION and four more syscalls.
//
// The drain is the only thread emptying the socket, and Dhan skips a slow
// consumer forward to "the latest available state" with no sequence number.
// So a stalled drain does not merely delay a candle: it loses ticks UPSTREAM,
// where no counter of ours can see them.
//
// The behaviour lives in `seal_writer_runner`'s unit tests, which drive a real
// thread over real tempdirs. These scans cover the half those cannot: that
// boot actually splits the offload, spawns a thread for it, and that shutdown
// can still reach that thread. A refactor that dropped any one of the three
// would leave every behavioural test green while the drain quietly went back
// to writing to disk itself.
// ---------------------------------------------------------------------------

#[test]
fn boot_splits_the_escalation_offload_and_spawns_its_thread() {
    let main = strip_line_comments(&read("crates/app/src/main.rs"));
    assert!(
        main.contains("split_escalation_offload()"),
        "boot must split the escalation offload — without it `SealOverflow::escalate` writes \
         to disk INLINE on whatever task called it, which for all three call sites is the \
         frame drain"
    );
    assert!(
        main.contains("tv-seal-escalate"),
        "the escalation thread must be NAMED — an unnamed thread is invisible in a `top -H` \
         when the operator is trying to find what is stalling the drain"
    );
    assert!(
        squash(&main).contains("sink.run("),
        "the split sink must actually be driven by a thread; splitting it and never running \
         it fills a bounded queue and then falls back inline forever"
    );
}

#[test]
fn the_escalation_thread_is_reachable_from_shutdown() {
    // The exact defect the WAL spill writer carried until the same day: a
    // spawned thread whose handle is discarded and whose stop flag nothing
    // holds cannot be drained at exit, so the queue dies with the process and
    // no counter reports it.
    let main = strip_line_comments(&read("crates/app/src/main.rs"));
    for needle in [
        "SEAL_ESCALATION_STOP",
        "SEAL_ESCALATION_THREAD",
        "SEAL_ESCALATION_SHUTDOWN_BUDGET",
        "stop_flag()",
    ] {
        assert!(
            main.contains(needle),
            "{needle} missing — the escalation thread's shutdown drain is unreachable, so \
             every seal still queued at exit is lost silently"
        );
    }
    assert!(
        main.contains("SEAL_ESCALATION_ABANDONED_COUNTER"),
        "a drain that runs out of budget must COUNT what it abandoned; an unreported \
         abandonment is the false-OK this whole shutdown path was rebuilt to stop producing"
    );
}

#[test]
fn a_refused_handoff_falls_back_inline_rather_than_dropping() {
    // `Full` and `Disconnected` both still hold the seal, so both must take
    // the old inline route. A `_ =>` arm that discarded here would reintroduce
    // the 2026-08-19 defect one layer further in, where the source scans above
    // (which watch `dhan_feed_stack`) cannot see it.
    let runner = strip_line_comments(&read("crates/storage/src/seal_writer_runner.rs"));
    let code = squash(&runner);
    assert!(
        code.contains(
            "TrySendError::Full(item) | std::sync::mpsc::TrySendError::Disconnected(item)"
        ) || code.contains(
            "TrySendError::Full(item)| std::sync::mpsc::TrySendError::Disconnected(item)"
        ),
        "both refusal arms must be handled together and must keep the item — a discard here \
         is a silent seal loss that no `dhan_feed_stack` scan can reach"
    );
    assert!(
        code.contains("SEAL_ESCALATION_INLINE_FALLBACK_COUNTER).increment(1)"),
        "the inline fallback must be counted, or a permanently-behind escalation thread looks \
         identical to a healthy one"
    );
}

#[test]
fn the_deferred_loss_still_pages() {
    // The caller is told `Queued`, so the AGGREGATOR-DROP-01 page for a seal
    // both disk tiers refuse can ONLY come from the escalation thread. If the
    // callback were dropped, a genuinely lost candle would page nobody —
    // strictly worse than the inline version this replaced.
    let main = strip_line_comments(&read("crates/app/src/main.rs"));
    let code = squash(&main);
    assert!(
        code.contains("sink.run(|seal| {") || code.contains("sink.run(|seal|{"),
        "the escalation thread must pass an on_lost hook, not one that ignores the seal"
    );
    assert!(
        main.contains("SealLossReason::BothDiskTiersFailed"),
        "the on_lost hook must fire the both-tiers-failed page from the thread"
    );
    let stack = strip_line_comments(&read("crates/app/src/dhan_feed_stack.rs"));
    assert!(
        stack.contains("OverflowOutcome::Queued"),
        "the caller must handle the Queued outcome explicitly — a wildcard arm would let a \
         future outcome variant silently classify as a loss, or as a rescue"
    );
}

#[test]
fn guard_self_test_offload_scans_can_bite() {
    // Every assertion above is a substring scan, so each is worthless if the
    // needle it looks for could never be absent. Prove the shapes on synthetic
    // sources rather than trusting the real ones.
    let without = "let overflow = runner.overflow();";
    assert!(
        !without.contains("split_escalation_offload()"),
        "self-test: a boot that does NOT split the offload must fail the scan"
    );
    let discarding = squash("Err(_) => { dropped += 1; }");
    assert!(
        !discarding.contains("SEAL_ESCALATION_INLINE_FALLBACK_COUNTER).increment(1)"),
        "self-test: a discarding refusal arm must fail the fallback scan"
    );
    let silent = squash("sink.run(|_| {});");
    assert!(
        !silent.contains("sink.run(|seal| {"),
        "self-test: an on_lost hook that ignores the seal must fail the paging scan"
    );
}

#[test]
fn the_escalation_spawn_is_idempotent_like_its_two_neighbours() {
    // `spawn_seal_writer_loop` installs three things in sequence — the seal
    // sender, the overflow escalator, and (since 2026-08-28) the escalation
    // thread. The first two are each written to survive a second entry with
    // an "idempotent skip" warn. The third was not, and that asymmetry is the
    // defect: unguarded, a second entry spawns a SECOND thread, overwrites
    // SEAL_ESCALATION_THREAD, and orphans the FIRST — whose stop flag is
    // unreachable, because the OnceLock already holds the first one. Shutdown
    // would then signal a thread it never joins and join a thread it never
    // signalled, and whatever the orphan still held would die with the
    // process, uncounted.
    //
    // Not reachable today (one production call site), which is exactly why it
    // needs a guard rather than a comment: the thing that makes it
    // unreachable is a fact about a caller, not about this code.
    let main = strip_line_comments(&read("crates/app/src/main.rs"));
    let code = squash(&main);
    assert!(
        code.contains("if SEAL_ESCALATION_STOP.set(sink.stop_flag()).is_err() {"),
        "the escalation spawn must be gated on the OnceLock set succeeding — an ignored \
         `let _ = ...set(...)` spawns an unstoppable orphan on a second entry"
    );
    assert!(
        code.contains("drop(sink);"),
        "the already-installed arm must DROP the sink, which disconnects the new sender so \
         that overflow escalates inline — the documented lossless fallback. Leaving the sink \
         alive would queue into a channel no thread is draining."
    );

    // Self-test: the scan can bite.
    let ignored = squash("let _ = SEAL_ESCALATION_STOP.set(sink.stop_flag());");
    assert!(
        !ignored.contains("if SEAL_ESCALATION_STOP.set(sink.stop_flag()).is_err() {"),
        "self-test: an ignored set() must fail the scan"
    );
}
