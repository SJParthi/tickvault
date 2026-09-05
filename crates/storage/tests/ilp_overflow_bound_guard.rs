//! Pins that every ILP writer which RETAINS rows across a failed flush also
//! BOUNDS what it retains, and that the loss counters involved reach
//! CloudWatch.
//!
//! # The defect this prevents
//!
//! `questdb-rs`' `Sender::flush` is `flush_impl(buf, ..)?; buf.clear()`. The
//! `?` returns before the clear, so a failed flush keeps every row that did
//! not go out. That is deliberate — a two-second QuestDB reconnect should not
//! cost an audit row — and four writers in this crate relied on it with no
//! bound at all.
//!
//! Unbounded retention fails in two ways, both silent:
//!
//! * a sustained outage grows the buffer for as long as it lasts, because the
//!   caller appends the next row into the same buffer and flushes again;
//! * a SERVER-SIDE reject (a bad column type, a schema drift) is rejected
//!   identically on every retry, so the buffer is poisoned and the table is
//!   dead for the process lifetime — while the log line reads like a
//!   transient failure.
//!
//! Neither is visible from the call site: the code that leaks is the code that
//! is NOT there. A source scan is the only thing that can see an absence.

const OVERFLOW_SRC: &str = include_str!("../src/ilp_overflow.rs");
const AGENT_JSON: &str = include_str!("../../../deploy/aws/cloudwatch-agent.json");
/// The deployed CloudWatch agent config (was embedded in
/// `user-data.sh.tftpl` until 2026-08-25 — that duplicate pinned the template
/// at zero free bytes and was removed; user-data now copies this file after the
/// Step 5 clone).
const USER_DATA: &str = include_str!("../../../deploy/aws/cloudwatch-agent.json");

/// Every writer that propagates a flush error while keeping its buffer.
///
/// 2026-08-21: this list held FOUR. `brutex_crossverify_persistence` was
/// removed along with the BruteX cross-verify comparator itself -- that
/// comparator read a table only the Groww feed could fill, so with one broker
/// it could only ever compare against nothing. A retention bound on a writer
/// that no longer exists is not coverage.
const BOUNDED_WRITERS: [(&str, &str); 4] = [
    (
        "ws_event_audit_persistence",
        include_str!("../src/ws_event_audit_persistence.rs"),
    ),
    (
        "feed_episode_audit_persistence",
        include_str!("../src/feed_episode_audit_persistence.rs"),
    ),
    (
        "feed_scoreboard_persistence",
        include_str!("../src/feed_scoreboard_persistence.rs"),
    ),
    // 2026-09-05: found MISSING by `the_bounded_writer_list_names_every_writer_that_calls_the_bound`
    // on its first run. It had been calling `discard_if_overflowing` while absent
    // from this array, which is the proof the hardcoded list was not maintained.
    (
        "ws_connection_daily_persistence",
        include_str!("../src/ws_connection_daily_persistence.rs"),
    ),
];

#[test]
fn every_retaining_writer_bounds_what_it_retains() {
    let unbounded: Vec<&str> = BOUNDED_WRITERS
        .iter()
        .filter(|(_, src)| !src.contains("ilp_overflow::discard_if_overflowing"))
        .map(|(name, _)| *name)
        .collect();

    assert!(
        unbounded.is_empty(),
        "these ILP writers retain rows across a failed flush without bounding \
         the retention: {unbounded:?}\n\n\
         A sustained QuestDB outage grows their buffer with no cap, and a \
         SERVER-SIDE reject poisons it permanently — the same rows are \
         re-sent and re-rejected forever, so the table is dead for the process \
         lifetime while the log reads like a transient failure. Call \
         `ilp_overflow::discard_if_overflowing` from the flush-failure arm."
    );
}

/// Every flushing writer in the crate must DECLARE how it treats its buffer on
/// a failed flush — discovered, not listed.
///
/// # Why this exists beside the test above
///
/// That test iterates `BOUNDED_WRITERS`, a hardcoded array. Its name promises
/// "EVERY retaining writer", and an adversarial sweep on 2026-09-05 showed the
/// promise was not kept: seventeen files in `crates/storage/src` define a
/// flush, the array named three, and `ws_connection_daily_persistence` was
/// already calling `discard_if_overflowing` without being in it — proof the
/// list was not being maintained. A new `*_persistence.rs` that retained an
/// unbounded buffer across a failed flush would have been green forever.
///
/// A list cannot answer a question about a set it does not enumerate. So this
/// test enumerates the set and requires each member to fall into exactly one
/// of two documented treatments:
///
///   * **bounded retain** — calls `ilp_overflow::discard_if_overflowing`, so
///     the buffer survives a transient outage but cannot grow without limit;
///   * **discard** — calls `discard_pending`, so nothing is retained at all.
///
/// A writer with NEITHER retains without a bound, which is the defect. There
/// is deliberately no exemption list: the two treatments are exhaustive, and
/// an exemption would be the same staleness one level up.
#[test]
fn every_flushing_writer_declares_its_failure_treatment() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = std::fs::read_dir(&src_dir).expect("crates/storage/src must be readable");

    let mut flushing = 0usize;
    let mut bounded = 0usize;
    let mut undeclared: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with("_persistence.rs") {
            continue;
        }
        // Lossy: one bad byte must not silently drop a writer from the sweep.
        let bytes = std::fs::read(&path).expect("a listed source file must be readable");
        let body = String::from_utf8_lossy(&bytes);
        if !body.contains("fn flush") {
            continue;
        }
        flushing += 1;

        let bounded_retain = body.contains("discard_if_overflowing");
        let discards = body.contains("discard_pending");
        if bounded_retain {
            bounded += 1;
        }
        if !bounded_retain && !discards {
            undeclared.push(name.to_owned());
        }
    }

    assert!(
        flushing >= 15,
        "found only {flushing} flushing writers — the discovery is broken and \
         this test would pass vacuously"
    );
    assert!(
        bounded >= 3,
        "found only {bounded} bounded-retain writers — the discriminator is \
         broken; every writer would read as 'discards' and nothing would be \
         checked"
    );
    assert!(
        undeclared.is_empty(),
        "these ILP writers flush but declare NO treatment for a failed flush: \
         {undeclared:?}\n\n\
         A writer that neither bounds its retention \
         (`ilp_overflow::discard_if_overflowing`) nor drops it \
         (`discard_pending`) keeps appending into a buffer that a sustained \
         QuestDB outage grows without limit — and a server-side reject poisons \
         it permanently, so the same rows are re-sent and re-rejected for the \
         process lifetime while the log reads like a transient failure.\n\n\
         Pick one and call it from the flush-failure arm. There is no \
         exemption list here on purpose: the two treatments are exhaustive, \
         and an exemption would go stale exactly like the hardcoded array this \
         test was added to replace."
    );
}

/// `BOUNDED_WRITERS` must not go stale in the other direction either: every
/// file that actually calls the bound has to be in it.
///
/// This is the half that was already broken when the sweep looked —
/// `ws_connection_daily_persistence` called `discard_if_overflowing` and was
/// not listed, so the array had silently stopped describing the code.
#[test]
fn the_bounded_writer_list_names_every_writer_that_calls_the_bound() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = std::fs::read_dir(&src_dir).expect("crates/storage/src must be readable");

    let mut missing: Vec<String> = Vec::new();
    let mut found = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with("_persistence.rs") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("a listed source file must be readable");
        if !String::from_utf8_lossy(&bytes).contains("discard_if_overflowing") {
            continue;
        }
        found += 1;
        let stem = name.trim_end_matches(".rs");
        if !BOUNDED_WRITERS.iter().any(|(listed, _)| *listed == stem) {
            missing.push(stem.to_owned());
        }
    }

    assert!(
        found >= 4,
        "found only {found} writers calling the bound — the discovery is broken"
    );
    assert!(
        missing.is_empty(),
        "these writers call `discard_if_overflowing` but are NOT in \
         BOUNDED_WRITERS: {missing:?}\n\n\
         The array has stopped describing the code, so the test that iterates \
         it is checking a subset while its name promises every writer. Add \
         them."
    );
}

#[test]
fn the_bound_discards_loudly_rather_than_silently() {
    assert!(
        OVERFLOW_SRC.contains("metrics::counter!(PENDING_DISCARDED_COUNTER"),
        "the overflow bound must COUNT what it discards. A bound that drops \
         rows silently trades an unbounded leak for an invisible loss, which \
         is the worse of the two — the leak at least shows up as memory."
    );
    assert!(
        OVERFLOW_SRC.contains("buffer.clear()"),
        "the bound must actually clear the buffer; resetting only the caller's \
         count would leave the poisoned rows in place AND lose the ability to \
         detect the next overflow."
    );
}

#[test]
fn the_loss_counters_reach_cloudwatch_via_both_selector_copies() {
    // `tv_depth_rows_dropped_total` and `tv_depth_persist_errors_total` were
    // emitted for months and shipped by nothing, while the rows-WRITTEN
    // counter beside them was shipped — so depth read healthy off-box while
    // its losses were unobservable. That asymmetry is the false-OK shape.
    for metric in [
        "tv_ilp_rows_discarded_total",
        "tv_depth_rows_dropped_total",
        "tv_depth_persist_errors_total",
    ] {
        for (label, src) in [
            ("deploy/aws/cloudwatch-agent.json", AGENT_JSON),
            ("deploy/aws/cloudwatch-agent.json", USER_DATA),
        ] {
            assert!(
                src.contains(metric),
                "`{metric}` is missing from {label}. It is emitted by production \
                 code and would be invisible off-box — a loss nobody can see is \
                 indistinguishable from no loss at all."
            );
        }
    }
}

#[test]
fn the_order_and_position_discard_counters_are_distinguishable() {
    // Both writers increment the SAME counter name. Without a label a lost
    // POSITION capture is reported as a lost ORDER capture, and the two have
    // different causes and different triage.
    let position = include_str!("../src/position_update_events_persistence.rs");
    let order = include_str!("../src/order_update_events_persistence.rs");
    assert!(
        position.contains(r#""kind" => "position""#),
        "the position writer must label its discards, or a position-capture \
         loss is indistinguishable from an order-capture loss in CloudWatch"
    );
    assert!(
        order.contains(r#""kind" => "order""#),
        "the order writer must carry the matching label — one side labelled \
         and the other bare folds into the same series and reads as a single \
         unlabelled total, which is the state this test exists to end"
    );
}

/// Every `*_rows_discarded_total` counter must be SEEDED at zero before its
/// writer can drop a row.
///
/// # Why an absent series is worse than a zero one
///
/// A counter never incremented does not appear on the local exporter at all,
/// and an absent series reads as "no data" — which a reader takes for health
/// when it may mean the single discard episode is the only sample there will
/// ever be. If the name is EMF-selected, it is worse still: the CloudWatch
/// agent computes counters as deltas and DROPS the first sample of a series it
/// has never seen, so the one episode that matters publishes nothing. That
/// exact rule cost 104,540 depth rows their classification on 2026-08-28.
///
/// Found by a sweep on 2026-09-05: five order/audit writers count rows at
/// APPEND time under a `*_rows_total` name, so the discard counter is the only
/// thing that can correct them — and two of the five had never registered
/// theirs. The subtraction that makes those counters honest was silently
/// unavailable for exactly the writers whose rows are hardest to re-create.
///
/// Discovered, not listed: a hardcoded set of writers is the staleness this
/// file already had to repair once today.
#[test]
fn every_discard_counter_is_seeded_before_its_writer_can_drop_a_row() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = std::fs::read_dir(&src_dir).expect("crates/storage/src must be readable");

    let mut unseeded: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".rs") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("a listed source file must be readable");
        let src = String::from_utf8_lossy(&bytes);

        // Every discard-counter NAME this file mentions, however it is spelled
        // at the emit site (bare, labelled, or via a helper).
        for metric in discard_counter_names(&src) {
            checked += 1;
            // The seed is the same name incremented by a literal zero. Matching
            // on the pair rather than on `increment(0)` alone is what stops a
            // seed for a DIFFERENT counter in the same file from counting.
            if GROWW_BRANCH_EXEMPT.contains(&metric.as_str()) {
                continue;
            }
            if is_seeded(&src, &metric) {
                continue;
            }
            unseeded.push(format!("{name}::{metric}"));
        }
    }

    assert!(
        checked >= 6,
        "found only {checked} discard counters — the discovery is broken, not \
         the code"
    );
    assert!(
        unseeded.is_empty(),
        "these discard counters are never registered at zero: {unseeded:?}\n\n\
         Add `metrics::counter!(\"<name>\").increment(0);` to the writer's \
         constructor, before any row can be appended. Until then the series is \
         ABSENT rather than zero — which reads as health on the local exporter, \
         and which the CloudWatch agent's dropped-first-sample rule turns into \
         total silence on the one episode that matters."
    );
}

/// Every `tv_*_rows_discarded_total` literal a source file mentions.
fn discard_counter_names(src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("\"tv_") {
        let after = &rest[i + 1..];
        if let Some(end) = after.find('"') {
            let name = &after[..end];
            if name.ends_with("_rows_discarded_total") && !out.iter().any(|n| n == name) {
                out.push(name.to_owned());
            }
            rest = &after[end..];
        } else {
            break;
        }
    }
    out
}

/// Discard counters whose emit site is on a branch with NO production caller.
///
/// The Groww feed was removed entirely on 2026-08-21 (operator directive,
/// recorded in `websocket-connection-scope-lock.md`). These three names sit on
/// the `feed == "groww"` arm of a per-feed name resolver, and every caller that
/// reaches that arm is a test: `OPTION_CHAIN_1M_FEED_GROWW` has zero
/// production occurrences outside its own definition and branch.
///
/// They are NOT seeded on purpose, and that is the opposite of an oversight. A
/// seeded counter publishes a permanently-zero series, and a permanently-zero
/// series for a feed that cannot write implies a live path that was deleted —
/// manufacturing exactly the false-OK the seeding rule exists to prevent.
///
/// The stale-entry test below is what stops this list becoming the staleness it
/// is exempting: an entry naming a counter no source file mentions any more
/// must be removed in the same change that removes the branch.
const GROWW_BRANCH_EXEMPT: [&str; 3] = [
    "tv_groww_chain1m_rows_discarded_total",
    "tv_groww_spot1m_rows_discarded_total",
    "tv_groww_contract1m_rows_discarded_total",
];

/// Whether `metric` is registered at zero somewhere in `src`.
///
/// Checks BOTH spellings, because the first version of this scan checked only
/// one and reported a false positive on the first run:
///
///   * the string literal, `counter!("name").increment(0)`; and
///   * a `const NAME: &str = "metric"` whose IDENTIFIER is what the seed
///     actually passes — the shape `ilp_overflow.rs` uses, and the shape that
///     made this scan claim a correctly-seeded counter was unseeded.
///
/// That literal-vs-const blind spot is the same one the EMF seeding sweep had
/// to be corrected for on 2026-09-05. A scan that only knows one spelling
/// reports on its own vocabulary, not on the code.
fn is_seeded(src: &str, metric: &str) -> bool {
    let mut needles: Vec<String> = vec![format!("\"{metric}\"")];
    // Resolve `const IDENT: &str = "metric";` to IDENT, so a seed written
    // against the constant counts.
    for (i, _) in src.match_indices(&format!("= \"{metric}\"")) {
        let head = &src[..i];
        if let Some(decl) = head.rfind("const ") {
            let after = &head[decl + "const ".len()..];
            if let Some((ident, _)) = after.split_once(':') {
                let ident = ident.trim();
                if !ident.is_empty() && !ident.contains(char::is_whitespace) {
                    needles.push(ident.to_owned());
                }
            }
        }
    }
    needles.iter().any(|n| {
        src.match_indices(n.as_str()).any(|(i, _)| {
            src[i..]
                .split_once(')')
                .is_some_and(|(_, r)| r.trim_start().starts_with(".increment(0)"))
        })
    })
}

/// The Groww exemptions must name counters that still exist.
///
/// Without this, deleting the Groww branch would leave three entries silently
/// exempting nothing — and the next counter that happened to take one of those
/// names would inherit an exemption nobody granted it.
#[test]
fn groww_branch_exemptions_each_name_a_counter_that_still_exists() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut all = String::new();
    for entry in std::fs::read_dir(&src_dir)
        .expect("crates/storage/src must be readable")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("a listed source file must be readable");
        all.push_str(&String::from_utf8_lossy(&bytes));
    }
    let stale: Vec<&str> = GROWW_BRANCH_EXEMPT
        .iter()
        .filter(|m| !all.contains(**m))
        .copied()
        .collect();
    assert!(
        stale.is_empty(),
        "STALE exemption(s) naming counters no source mentions: {stale:?}\n\n\
         Remove them in the same change that removed the branch, or the next \
         counter to take one of these names inherits an exemption nobody \
         granted it."
    );
}
