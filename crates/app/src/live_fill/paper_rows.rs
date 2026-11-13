//! The `live_order_events` rows of one paper order after the chain hands it
//! off (plan item LF-8): Sent, Ack, Fill, then Cancel or Reject.
//!
//! Lifted out of the order runtime's arm (`order_runtime_live_fill::execute`)
//! so the runtime and the offline replay (`live_fill::replay`) build the same
//! rows from the same [`PaperExecution`]. Pure: the caller passes the
//! timestamp (the runtime its wall clock at each step, the replay its virtual
//! clock), the order id and the round-trip time, so a replay of the same input
//! gives the same bytes.
//!
//! # Complexity
//!
//! Cold: per paper order, never per tick. Each row is one clone of the
//! signal's template (its text fields allocate); [`execution_rows`] writes at
//! most four rows. O(1).

use tickvault_storage::live_order_events_persistence::{LiveOrderEvent, LiveOrderEventKind};
use tickvault_trading::brutex_host::engine::Side;

use super::chain::PaperExecution;

/// One row from the signal's template: `kind`, the next `seq` (then moved
/// on), stamped at `ts_ist_nanos`. The caller fills the step's own fields.
/// O(1) plus one clone.
#[must_use]
pub fn order_row(
    base: &LiveOrderEvent,
    ts_ist_nanos: i64,
    kind: LiveOrderEventKind,
    seq: &mut i32,
) -> LiveOrderEvent {
    let mut row = base.clone();
    row.ts_ist_nanos = ts_ist_nanos;
    row.event = kind;
    row.seq = *seq;
    *seq = seq.saturating_add(1);
    row
}

/// The Sent row's own fields. O(1).
pub fn fill_sent(row: &mut LiveOrderEvent, order_id: &str, api_rtt_us: Option<i64>) {
    row.order_id = Some(order_id.to_owned());
    row.api_rtt_us = api_rtt_us;
}

/// The Ack row's own fields. O(1).
pub fn fill_ack(row: &mut LiveOrderEvent, order_id: &str) {
    row.order_id = Some(order_id.to_owned());
}

/// The Fill row's own fields: `filled` shares at the model's price, which IS
/// the paper fill, so it is never worse than the model. O(1).
pub fn fill_fill(row: &mut LiveOrderEvent, order_id: &str, x: &PaperExecution, filled: i64) {
    row.order_id = Some(order_id.to_owned());
    row.fill_qty = Some(filled);
    row.fill_price_paise = x.avg_price_paise;
    row.avg_price_paise = x.avg_price_paise;
    row.remainder_qty = Some(x.qty.saturating_sub(filled));
    // The paper fill IS the worst-case model's price.
    row.worse_than_model = Some(false);
}

/// The Cancel row's own fields. O(1).
pub fn fill_cancel(row: &mut LiveOrderEvent, order_id: &str, remainder: i64, reason: String) {
    row.order_id = Some(order_id.to_owned());
    row.remainder_qty = Some(remainder);
    row.reason = Some(reason);
}

/// A Reject row's own fields: the whole order left unfilled. O(1).
pub fn fill_reject(row: &mut LiveOrderEvent, x: &PaperExecution, reason: String) {
    row.remainder_qty = Some(x.qty.max(0));
    row.reason = Some(reason);
}

/// The reason on the Cancel of a remainder: nothing filled, or a part. O(1).
#[must_use]
pub const fn remainder_reason(filled: i64) -> &'static str {
    if filled == 0 {
        "no_fill_in_window"
    } else {
        "remainder_after_window"
    }
}

/// A Reject row for `x` at `ts`. O(1) plus one clone.
#[must_use]
pub fn reject_row(x: &PaperExecution, ts: i64, reason: String, seq: &mut i32) -> LiveOrderEvent {
    let mut row = order_row(&x.base, ts, LiveOrderEventKind::Reject, seq);
    fill_reject(&mut row, x, reason);
    row
}

/// What happened to a placed paper order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperOutcome {
    /// The dry-run OMS id (`PAPER-n`).
    pub order_id: String,
    /// Place round trip, µs; `None` where none was timed (the replay).
    pub api_rtt_us: Option<i64>,
    /// Shares booked (0 when nothing filled).
    pub filled: i64,
    /// For a remainder: `Ok(cancel reason)`, or `Err(reject reason)` when it
    /// could not be closed. Ignored when the order filled whole.
    pub closed: Result<String, String>,
}

/// The rows of a placed paper order, in the runtime's order: Sent, Ack, a
/// Fill when something filled, then a Cancel (or Reject) when a remainder was
/// left. All stamped at `out_ts_ist_ns`. O(1): at most four rows.
pub fn execution_rows(
    x: &PaperExecution,
    out_ts_ist_ns: i64,
    outcome: &PaperOutcome,
    seq: &mut i32,
    out: &mut Vec<LiveOrderEvent>,
) {
    let id = outcome.order_id.as_str();
    let mut sent = order_row(&x.base, out_ts_ist_ns, LiveOrderEventKind::Sent, seq);
    fill_sent(&mut sent, id, outcome.api_rtt_us);
    out.push(sent);
    let mut ack = order_row(&x.base, out_ts_ist_ns, LiveOrderEventKind::Ack, seq);
    fill_ack(&mut ack, id);
    out.push(ack);
    if outcome.filled > 0 {
        let mut fill = order_row(&x.base, out_ts_ist_ns, LiveOrderEventKind::Fill, seq);
        fill_fill(&mut fill, id, x, outcome.filled);
        out.push(fill);
    }
    if outcome.filled < x.qty {
        let remainder = x.qty.saturating_sub(outcome.filled);
        match &outcome.closed {
            Ok(reason) => {
                let mut cancel = order_row(&x.base, out_ts_ist_ns, LiveOrderEventKind::Cancel, seq);
                fill_cancel(&mut cancel, id, remainder, reason.clone());
                out.push(cancel);
            }
            Err(reason) => out.push(reject_row(x, out_ts_ist_ns, reason.clone(), seq)),
        }
    }
}

/// An exit sized to the opposite-signed paper position held (`held_lots`,
/// signed): `None` when nothing is held on the side it would close, else the
/// execution with its order, fill and row quantities cut to the position.
/// Cold, one clone per exit.
#[must_use]
pub fn clamp_exit(held_lots: i32, x: &PaperExecution) -> Option<PaperExecution> {
    let can_lots = match x.side {
        Side::Sell if held_lots > 0 => i64::from(held_lots),
        Side::Buy if held_lots < 0 => i64::from(held_lots).saturating_neg(),
        _ => 0,
    };
    if can_lots == 0 {
        return None;
    }
    let can = can_lots.saturating_mul(i64::from(x.lot_size.max(1)));
    let mut c = x.clone();
    c.qty = c.qty.min(can);
    c.filled_qty = c.filled_qty.min(c.qty);
    c.base.qty = c.qty;
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_fill::test_support;
    use tickvault_common::types::ExchangeSegment;

    fn execution(qty: i64, filled: i64, exit: bool) -> PaperExecution {
        let mut base = test_support::event();
        base.qty = qty;
        PaperExecution {
            base,
            next_seq: 2,
            security_id: 1,
            segment: ExchangeSegment::NseEquity,
            side: Side::Sell,
            qty,
            lot_size: 10,
            limit_price: 99.5,
            filled_qty: filled,
            avg_price: 100.0,
            avg_price_paise: Some(10_000),
            exit,
        }
    }

    /// The rows the runtime's `emit` closures wrote before 2026-11-12, kept as
    /// the oracle (the wall clock replaced by `ts`).
    fn previous_emit_rows(
        x: &PaperExecution,
        ts: i64,
        order_id: &str,
        rtt: Option<i64>,
        filled: i64,
        closed: &Result<String, String>,
    ) -> Vec<LiveOrderEvent> {
        let mut seq = x.next_seq;
        let mut out = Vec::new();
        let mut emit = |kind, f: &dyn Fn(&mut LiveOrderEvent), seq: &mut i32| {
            let mut row = x.base.clone();
            row.ts_ist_nanos = ts;
            row.event = kind;
            row.seq = *seq;
            *seq = seq.saturating_add(1);
            f(&mut row);
            out.push(row);
        };
        emit(
            LiveOrderEventKind::Sent,
            &|r| {
                r.order_id = Some(order_id.to_owned());
                r.api_rtt_us = rtt;
            },
            &mut seq,
        );
        emit(
            LiveOrderEventKind::Ack,
            &|r| r.order_id = Some(order_id.to_owned()),
            &mut seq,
        );
        if filled > 0 {
            emit(
                LiveOrderEventKind::Fill,
                &|r| {
                    r.order_id = Some(order_id.to_owned());
                    r.fill_qty = Some(filled);
                    r.fill_price_paise = x.avg_price_paise;
                    r.avg_price_paise = x.avg_price_paise;
                    r.remainder_qty = Some(x.qty.saturating_sub(filled));
                    r.worse_than_model = Some(false);
                },
                &mut seq,
            );
        }
        if filled < x.qty {
            let remainder = x.qty.saturating_sub(filled);
            match closed {
                Ok(reason) => emit(
                    LiveOrderEventKind::Cancel,
                    &|r| {
                        r.order_id = Some(order_id.to_owned());
                        r.remainder_qty = Some(remainder);
                        r.reason = Some(reason.clone());
                    },
                    &mut seq,
                ),
                Err(reason) => emit(
                    LiveOrderEventKind::Reject,
                    &|r| {
                        r.remainder_qty = Some(x.qty.max(0));
                        r.reason = Some(reason.clone());
                    },
                    &mut seq,
                ),
            }
        }
        out
    }

    #[test]
    fn test_execution_rows_equal_previous_emit_rows() {
        let cases: [(i64, i64, Result<String, String>); 5] = [
            (20, 20, Ok(remainder_reason(20).to_owned())), // full fill
            (20, 10, Ok(remainder_reason(10).to_owned())), // partial + cancel
            (20, 0, Ok(remainder_reason(0).to_owned())),   // no fill
            (
                20,
                10,
                Ok("remainder_after_window;oms_cancel_refused:rate".to_owned()),
            ), // local close
            (
                20,
                0,
                Err("cancel_refused:x;local_close_failed:y".to_owned()),
            ), // reject
        ];
        for (qty, filled, closed) in cases {
            let x = execution(qty, filled, false);
            let outcome = PaperOutcome {
                order_id: "PAPER-7".to_owned(),
                api_rtt_us: Some(42),
                filled,
                closed: closed.clone(),
            };
            let mut seq = x.next_seq;
            let mut got = Vec::new();
            execution_rows(&x, 1_234, &outcome, &mut seq, &mut got);
            let want = previous_emit_rows(&x, 1_234, "PAPER-7", Some(42), filled, &closed);
            assert_eq!(got, want, "qty {qty} filled {filled}");
            assert_eq!(seq, x.next_seq + i32::try_from(want.len()).expect("fits"));
        }
    }

    #[test]
    fn test_reject_row_and_order_row_stamp_seq_and_ts() {
        let x = execution(30, 0, false);
        let mut seq = 2;
        let r = reject_row(&x, 9, "risk:halt".to_owned(), &mut seq);
        assert_eq!(seq, 3);
        assert_eq!(r.ts_ist_nanos, 9);
        assert_eq!(r.event, LiveOrderEventKind::Reject);
        assert_eq!(r.remainder_qty, Some(30));
        assert_eq!(r.reason.as_deref(), Some("risk:halt"));
        let mut s = 5;
        let o = order_row(&x.base, 11, LiveOrderEventKind::Ack, &mut s);
        assert_eq!((o.seq, s, o.ts_ist_nanos), (5, 6, 11));
        let mut a = o.clone();
        fill_ack(&mut a, "PAPER-1");
        fill_sent(&mut a, "PAPER-1", None);
        fill_cancel(&mut a, "PAPER-1", 3, "x".to_owned());
        fill_fill(&mut a, "PAPER-1", &x, 0);
        assert_eq!(a.order_id.as_deref(), Some("PAPER-1"));
    }

    #[test]
    fn test_fill_sent_fill_ack_fill_fill_fill_cancel_fill_reject_set_their_fields() {
        let x = execution(40, 30, false);
        let mut r = x.base.clone();
        fill_sent(&mut r, "PAPER-2", Some(9));
        assert_eq!(
            (r.order_id.as_deref(), r.api_rtt_us),
            (Some("PAPER-2"), Some(9))
        );
        let mut r = x.base.clone();
        fill_ack(&mut r, "PAPER-2");
        assert_eq!(r.order_id.as_deref(), Some("PAPER-2"));
        let mut r = x.base.clone();
        fill_fill(&mut r, "PAPER-2", &x, 30);
        assert_eq!(
            (r.fill_qty, r.remainder_qty, r.avg_price_paise),
            (Some(30), Some(10), Some(10_000))
        );
        assert_eq!(r.worse_than_model, Some(false));
        let mut r = x.base.clone();
        fill_cancel(&mut r, "PAPER-2", 10, "remainder_after_window".to_owned());
        assert_eq!(
            (r.remainder_qty, r.reason.as_deref()),
            (Some(10), Some("remainder_after_window"))
        );
        let mut r = x.base.clone();
        fill_reject(&mut r, &x, "risk:halt".to_owned());
        assert_eq!(
            (r.remainder_qty, r.reason.as_deref()),
            (Some(40), Some("risk:halt"))
        );
    }

    #[test]
    fn test_remainder_reason_names_none_or_part() {
        assert_eq!(remainder_reason(0), "no_fill_in_window");
        assert_eq!(remainder_reason(1), "remainder_after_window");
    }

    #[test]
    fn test_clamp_exit_cuts_to_the_held_position() {
        let x = execution(50, 50, true);
        assert!(clamp_exit(0, &x).is_none(), "nothing held");
        assert!(clamp_exit(-2, &x).is_none(), "a sell exit needs a long");
        let c = clamp_exit(2, &x).expect("long held");
        assert_eq!((c.qty, c.filled_qty, c.base.qty), (20, 20, 20));
    }
}
