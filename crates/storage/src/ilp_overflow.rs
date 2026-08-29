//! Bounds the ILP write buffer of a writer that RETAINS rows across a failed
//! flush, so retention cannot become unbounded growth.
//!
//! # The defect this exists for
//!
//! `questdb-rs`' `Sender::flush` is `flush_impl(buf, ..)?; buf.clear()` — the
//! `?` means a failed flush returns before the clear, so the buffer keeps
//! every row that did not go out. That is deliberate and usually right: a
//! QuestDB blip should not cost an audit row, and the next flush re-sends
//! them.
//!
//! It stops being right in two cases, and both are silent:
//!
//! 1. **A sustained outage.** The caller appends the next row into the same
//!    buffer and flushes again. Rows accumulate for as long as the outage
//!    lasts, with no cap, no eviction and no counter — an unbounded SPACE
//!    growth on a path whose own docs promise the opposite.
//! 2. **A server-side reject.** ILP-over-HTTP surfaces those in the ACK (TCP
//!    never did). A rejected row — a bad column type, a schema drift — is
//!    rejected *identically* on every retry, so the buffer is poisoned: it
//!    grows forever and the table is dead for the process lifetime, while the
//!    consumer logs a flush failure that reads like a transient one.
//!
//! # Why a cap rather than discard-on-first-failure
//!
//! `order_audit_persistence` discards on the first failure and counts it. That
//! is the safe shape for the SEBI order chain, where a poisoned buffer would
//! stall the order path. For the append-only audit tables here, discarding
//! immediately would throw away rows that a two-second QuestDB reconnect would
//! have delivered — trading a real loss for a hypothetical one.
//!
//! A cap gets both: retries keep working for anything shorter than
//! [`MAX_PENDING_ROWS`] rows' worth of outage, and past that the buffer is
//! discarded LOUDLY so memory is bounded and a poisoned table recovers instead
//! of staying dead. The failure is fail-closed on space and never silent.
//!
//! # Complexity
//!
//! O(1) — one comparison, and on overflow one `Buffer::clear` plus a counter
//! increment. No allocation.

use questdb::ingress::Buffer;

/// How many rows a writer may retain across failed flushes before the buffer
/// is discarded.
///
/// Sized against the widest audit row in this crate (~300 B) so a full buffer
/// is ~30 MB — large enough to ride out a multi-minute QuestDB outage on every
/// one of these low-rate tables at once, and small enough that it cannot
/// meaningfully eat into the 32 GiB host budget. It is a memory bound, not a
/// durability promise: the tables it guards are append-only forensics, and
/// their consumers already page on flush failure.
pub const MAX_PENDING_ROWS: usize = 100_000;

/// Counter for rows discarded by the overflow bound.
///
/// ONE name with a `table` label rather than four names: the EMF processor
/// folds labels to `{host}` by summing, so a single selector entry alarms on
/// "any audit writer overflowed" while the label survives in the log line,
/// which is where triage reads it. Four names would cost four selector
/// entries to say the same thing.
pub const PENDING_DISCARDED_COUNTER: &str = "tv_ilp_rows_discarded_total";

/// Put this writer's discard series on the wire at zero, from its constructor.
///
/// # Why a call is needed at all — measured, not assumed
///
/// A `cloudwatch list-metrics` sweep on 2026-08-29 compared the EMF selector
/// against the live account: the selector names 104 metrics, the account held
/// 86, and `tv_ilp_rows_discarded_total` was among the names that had **never
/// published a single datapoint** — despite being selected, and therefore paid
/// for, the whole time.
///
/// The mechanism is worth stating exactly. The CloudWatch agent computes a
/// counter as the delta between consecutive samples and drops the first sample
/// of a series it has never seen, so a counter that is only touched when
/// something breaks is *born at the breakage* and its first — and possibly
/// only — episode is the sample that gets discarded. Worse, **an absent series
/// is indistinguishable from a healthy zero one**: "no audit rows were ever
/// discarded" and "this writer never ran" render identically, and an alarm
/// over the metric would sit in `OK` forever without ever being able to fire.
///
/// # Why each writer calls it, when the label folds anyway
///
/// The EMF processor folds label values into one summed series per host, so
/// any single call would make the NAME visible. Each writer still seeds its
/// own label, because the label survives in the log line where triage reads
/// it, and because seeding from a writer's own constructor is what keeps the
/// series honest: it appears when that writer exists and not before. A central
/// boot-time seeder would publish a confident zero for a writer that is not
/// running, which is a worse false-OK than the silence it replaces.
/// `pub(crate)`, not `pub`: every caller is a writer inside this crate, so this
/// is internal plumbing rather than public API surface.
pub(crate) fn register_overflow_baseline(table: &'static str) {
    metrics::counter!(PENDING_DISCARDED_COUNTER, "table" => table).increment(0);
}

/// Call from a writer's flush-failure arm. Returns the number of rows
/// discarded — `0` in the normal case, where the rows are retained for the
/// next attempt.
///
/// `pending` is the caller's own row count and is reset to `0` on discard, so
/// the two can never disagree about what the buffer holds.
pub fn discard_if_overflowing(
    buffer: &mut Buffer,
    pending: &mut usize,
    table: &'static str,
) -> usize {
    if *pending < MAX_PENDING_ROWS {
        return 0;
    }
    let dropped = *pending;
    metrics::counter!(PENDING_DISCARDED_COUNTER, "table" => table).increment(dropped as u64);
    buffer.clear();
    *pending = 0;
    dropped
}

/// The context line for a failed flush, naming whether the bound discarded.
///
/// Extracted from the four writers rather than repeated in each, for two
/// reasons. The dull one is that four copies of a message drift. The real one
/// is coverage: the `dropped > 0` branch needs a genuine ILP failure with a
/// full buffer behind it, which no unit test can stage without a QuestDB — so
/// four inline copies are four blocks of production code that testing can
/// never reach, and the crate's ratcheted coverage floor is what notices.
///
/// Here the wording is a pure function of two arguments and is tested
/// directly, so the writers keep only the part that genuinely needs a live
/// broker to exercise.
#[must_use]
pub fn flush_failure_context(what: &'static str, dropped: usize) -> String {
    if dropped > 0 {
        format!(
            "{what} failed and the retained buffer hit its {MAX_PENDING_ROWS}-row bound; \
             {dropped} row(s) were discarded so memory stays bounded and a poisoned \
             buffer cannot keep this table dead"
        )
    } else {
        format!("{what} failed; rows are RETAINED for the next attempt")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use questdb::ingress::ProtocolVersion;

    #[test]
    fn flush_failure_context_distinguishes_retained_from_discarded() {
        let retained = flush_failure_context("t ILP flush", 0);
        assert!(
            retained.contains("RETAINED"),
            "a failure below the bound must say the rows are still there — an \
             operator reading this decides whether data was lost, and the two \
             cases have opposite answers: {retained}"
        );
        assert!(
            !retained.contains("discarded"),
            "the retained case must not read as a loss: {retained}"
        );

        let discarded = flush_failure_context("t ILP flush", 123);
        assert!(
            discarded.contains("123 row(s) were discarded"),
            "a discard must name HOW MANY, or the log records that something \
             was lost without recording how much: {discarded}"
        );
        assert!(
            discarded.contains(&MAX_PENDING_ROWS.to_string()),
            "and it must name the bound that caused it, so the reader can tell \
             a capacity problem from a transient one: {discarded}"
        );
    }

    #[test]
    fn flush_failure_context_always_names_the_failing_operation() {
        for dropped in [0usize, MAX_PENDING_ROWS] {
            assert!(
                flush_failure_context("ws_event_audit ILP flush", dropped)
                    .starts_with("ws_event_audit ILP flush"),
                "four writers share this text, so the line is useless unless it \
                 says WHICH one failed"
            );
        }
    }

    #[test]
    fn discard_if_overflowing_retains_rows_below_the_cap() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut pending = MAX_PENDING_ROWS - 1;
        assert_eq!(
            discard_if_overflowing(&mut buffer, &mut pending, "t"),
            0,
            "below the cap the rows must survive — a two-second QuestDB \
             reconnect should cost nothing"
        );
        assert_eq!(pending, MAX_PENDING_ROWS - 1, "the count must be untouched");
    }

    #[test]
    fn discard_if_overflowing_discards_and_counts_at_the_cap() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut pending = MAX_PENDING_ROWS;
        assert_eq!(
            discard_if_overflowing(&mut buffer, &mut pending, "t"),
            MAX_PENDING_ROWS,
            "at the cap the buffer must be discarded and the count reported"
        );
        assert_eq!(
            pending, 0,
            "the caller's count must be reset with the buffer, or the two \
             disagree about what is held and the next overflow check is wrong"
        );
    }

    #[test]
    fn discard_if_overflowing_is_idempotent_once_drained() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut pending = MAX_PENDING_ROWS;
        discard_if_overflowing(&mut buffer, &mut pending, "t");
        assert_eq!(
            discard_if_overflowing(&mut buffer, &mut pending, "t"),
            0,
            "a second call on an already-drained buffer must not re-count"
        );
    }

    #[test]
    fn discard_if_overflowing_zero_pending_is_a_no_op() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        let mut pending = 0usize;
        assert_eq!(discard_if_overflowing(&mut buffer, &mut pending, "t"), 0);
        assert_eq!(pending, 0);
    }

    /// The seeder must be safe to call repeatedly and from several writers.
    ///
    /// This is a real property, not a formality: five writers call it from
    /// their own constructors, a process can build more than one of them, and
    /// nothing coordinates the order. It must therefore never panic and never
    /// depend on being the first caller. With no recorder installed — the
    /// state under `cargo test` — `metrics::counter!` resolves to a no-op
    /// recorder, so this also pins that seeding cannot fault a test binary or
    /// a boot that runs before the exporter is installed.
    #[test]
    fn register_overflow_baseline_is_repeatable_and_never_panics() {
        register_overflow_baseline("ws_event_audit");
        register_overflow_baseline("ws_event_audit");
        for table in [
            "ws_connection_daily",
            "feed_scoreboard_daily",
            "table_storage_daily",
            "feed_episode_audit",
        ] {
            register_overflow_baseline(table);
        }
    }

    /// A seed is an `increment(0)`, and it must stay one.
    ///
    /// An `increment(1)` here would publish a fabricated discard on every
    /// writer construction — turning the instrument that reports row loss into
    /// a source of it. Cheap to assert, and the failure it prevents is silent.
    #[test]
    fn the_baseline_seeds_with_zero_not_a_fabricated_count() {
        let source = include_str!("ilp_overflow.rs");
        let start = source
            .find("pub(crate) fn register_overflow_baseline(")
            .expect("the seeder must exist");
        let body = &source[start..(start + 260).min(source.len())];
        assert!(
            body.contains(".increment(0)"),
            "register_overflow_baseline must seed with increment(0); anything else \
             publishes a discard that never happened"
        );
    }
}
