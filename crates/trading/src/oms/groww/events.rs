//! The trading-side order-update HINT sink (design §4.11; ORD-PR-3).
//!
//! Session 3 (the order-update push feed, producer in `core`/`app`) will map
//! its broker payloads onto the neutral
//! [`tickvault_common::broker_order_events::BrokerOrderEvent`] and hand each
//! one, with its [`EventSource`], to a [`GrowwOrderHintSink`]. This module
//! owns ONLY the trading-side consumer contract — the event TYPE itself lives
//! in `common` (dependency flow: `common ← core ← trading`, so Session 3's
//! producer never depends on `tickvault-trading`).
//!
//! # The load-bearing invariant — a hint NEVER transitions state
//! A hint is UNTRUSTED and LOSSY (the broadcast transport drops under lag,
//! `RecvError::Lagged`). It therefore NEVER drives the [`super::state`] FSM
//! and NEVER mutates a [`super::state::TrackedOrderState`]. It does exactly
//! ONE thing: enqueue a targeted [`OrderPollHint`] so the executor issues a
//! budget-reserved `get_order_detail` read whose REST response — not the hint
//! — drives the FSM (trust-but-verify; reconciliation-as-truth). This module
//! deliberately imports nothing from [`super::state`]; the
//! `hint_never_transitions_state` ratchet (in the ORD-PR-3 tests) source-scans
//! that the FSM types never appear here — a future push-promotion must edit a
//! loud ratchet, not slip in silently.
//!
//! Cold path; the channel is bounded at [`HINT_CHANNEL_CAPACITY`] (the
//! `dhan_rest_stack` order-update precedent).

use tickvault_common::broker_order_events::{BrokerOrderEvent, EventSource};
use tokio::sync::broadcast;

/// The bounded hint channel capacity (the 256 order-update broadcast precedent
/// in `dhan_rest_stack`).
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub const HINT_CHANNEL_CAPACITY: usize = 256;

/// A targeted poll request — the ONLY thing a hint produces. Carries no
/// status, no fill, no transition: it names an order to RE-READ, nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderPollHint {
    /// The broker order id to issue a targeted `get_order_detail` read for
    /// (`None` when the push carried only a reference id — the executor
    /// resolves it via its own reference→order map).
    pub broker_order_id: Option<String>,
    /// The user reference id the push echoed, if any (the fallback key).
    pub reference_id: Option<String>,
    /// The provenance of the push that produced this hint (forensic only).
    pub source: EventSource,
}

impl OrderPollHint {
    /// Build a hint from a neutral broker event + its transport provenance.
    /// Pure projection — copies the two id fields; deliberately ignores
    /// status/fill/price (a hint carries NO adopted state).
    #[must_use]
    pub fn from_broker_event(event: &BrokerOrderEvent, source: EventSource) -> Self {
        Self {
            broker_order_id: if event.broker_order_id.is_empty() {
                None
            } else {
                Some(event.broker_order_id.clone())
            },
            reference_id: event.reference_id.clone(),
            source,
        }
    }
}

/// The trading-side sink Session 3's producer calls. Total + infallible from
/// the producer's view — a full/closed channel is absorbed (counted), never
/// propagated, so a lossy hint transport can never stall or fail the producer.
pub trait GrowwOrderHintSink: Send + Sync {
    /// Consume one neutral broker event + its provenance. The ONLY permitted
    /// action is to enqueue a targeted poll hint — implementers MUST NOT
    /// transition any order state here.
    fn on_broker_event(&self, event: &BrokerOrderEvent, source: EventSource);
}

/// A [`GrowwOrderHintSink`] that forwards a targeted [`OrderPollHint`] onto a
/// bounded broadcast channel for the executor's hint consumer to drain.
///
/// A closed/full send is absorbed + counted (`tv_groww_order_hints_total`) —
/// the executor's periodic reconcile is the safety floor when hints are lost.
#[derive(Debug, Clone)]
pub struct BroadcastHintSink {
    tx: broadcast::Sender<OrderPollHint>,
}

impl BroadcastHintSink {
    /// Wrap a broadcast sender as a hint sink.
    #[must_use]
    pub fn new(tx: broadcast::Sender<OrderPollHint>) -> Self {
        Self { tx }
    }

    /// A fresh bounded hint channel + this sink over its sender.
    #[must_use]
    pub fn channel() -> (Self, broadcast::Receiver<OrderPollHint>) {
        let (tx, rx) = hint_channel();
        (Self::new(tx), rx)
    }
}

impl GrowwOrderHintSink for BroadcastHintSink {
    fn on_broker_event(&self, event: &BrokerOrderEvent, source: EventSource) {
        let hint = OrderPollHint::from_broker_event(event, source);
        match self.tx.send(hint) {
            Ok(_receivers) => {
                metrics::counter!("tv_groww_order_hints_total", "outcome" => "enqueued")
                    .increment(1);
            }
            Err(_no_receivers) => {
                // No live receiver — the reconcile sweep is the floor. Absorb,
                // count, never propagate (a lossy hint never stalls Session 3).
                metrics::counter!("tv_groww_order_hints_total", "outcome" => "dropped")
                    .increment(1);
            }
        }
    }
}

/// Construct a bounded hint broadcast channel at [`HINT_CHANNEL_CAPACITY`].
#[must_use]
pub fn hint_channel() -> (
    broadcast::Sender<OrderPollHint>,
    broadcast::Receiver<OrderPollHint>,
) {
    broadcast::channel(HINT_CHANNEL_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::broker_order_events::BrokerOrderStatus;
    use tickvault_common::feed::Feed;

    fn sample_event() -> BrokerOrderEvent {
        BrokerOrderEvent {
            broker: Feed::Groww,
            broker_order_id: "GW-123".to_owned(),
            reference_id: Some("TV2607150001ABCD".to_owned()),
            status: BrokerOrderStatus::Open,
            raw_status: "OPEN".to_owned(),
            filled_qty: 0,
            remaining_qty: Some(50),
            avg_fill_price_paise: None,
            segment: "FNO".to_owned(),
            exchange_ts_ms: Some(1_700_000_000_000),
            received_at_ms: 1_700_000_000_100,
        }
    }

    #[test]
    fn hint_projects_only_the_two_ids_and_source() {
        let ev = sample_event();
        let hint = OrderPollHint::from_broker_event(&ev, EventSource::Push);
        assert_eq!(hint.broker_order_id.as_deref(), Some("GW-123"));
        assert_eq!(hint.reference_id.as_deref(), Some("TV2607150001ABCD"));
        assert_eq!(hint.source, EventSource::Push);
    }

    #[test]
    fn empty_order_id_projects_to_none() {
        let mut ev = sample_event();
        ev.broker_order_id = String::new();
        let hint = OrderPollHint::from_broker_event(&ev, EventSource::Poll);
        assert_eq!(hint.broker_order_id, None);
    }

    #[test]
    fn sink_enqueues_a_targeted_poll_hint_onto_the_channel() {
        let (sink, mut rx) = BroadcastHintSink::channel();
        let ev = sample_event();
        sink.on_broker_event(&ev, EventSource::Push);
        let got = rx.try_recv().expect("one hint enqueued");
        assert_eq!(got.broker_order_id.as_deref(), Some("GW-123"));
        assert_eq!(got.source, EventSource::Push);
    }

    #[test]
    fn sink_absorbs_a_send_with_no_receiver() {
        let (tx, rx) = hint_channel();
        drop(rx); // no live receiver
        let sink = BroadcastHintSink::new(tx);
        // Must NOT panic / propagate — the reconcile sweep is the floor.
        sink.on_broker_event(&sample_event(), EventSource::Poll);
    }

    #[test]
    fn channel_capacity_is_the_bounded_precedent() {
        assert_eq!(HINT_CHANNEL_CAPACITY, 256);
        let (_tx, _rx) = hint_channel();
    }
}
