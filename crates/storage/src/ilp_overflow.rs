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

#[cfg(test)]
mod tests {
    use super::*;
    use questdb::ingress::ProtocolVersion;

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
}
