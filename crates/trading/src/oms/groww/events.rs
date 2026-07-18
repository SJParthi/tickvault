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

// ---------------------------------------------------------------------------
// FULL-FIDELITY push-event fan-out (order-push Stage C, 2026-07-16).
//
// DELIBERATELY DISTINCT from the hint lane above: `OrderPollHint` /
// `BroadcastHintSink` are LOSSY BY CONTRACT (they project away
// status/fill/qty/price so a hint can never transition state). The push
// runner's downstream consumers (Stage D: audit rows, Telegram, the
// push_active flag owner) need the FULL `BrokerOrderEvent`, so they attach
// through the fan-out below — never through the hint lane. A future
// hint-lane subscriber is just one more sink that internally projects.
// ---------------------------------------------------------------------------

/// Bounded per-sink queue capacity for [`BoundedPushEventSink`] (the same
/// 256-deep order-update precedent as [`HINT_CHANNEL_CAPACITY`]).
pub const PUSH_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Whether one sink accepted one event (the fan-out counts the drops).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushSinkDelivery {
    /// The sink accepted the event.
    Delivered,
    /// The sink refused the event (full/closed queue) — counted by the
    /// fan-out, never blocking the runner's read loop.
    Dropped,
}

/// A FULL-FIDELITY consumer of push order events. Implementations MUST be
/// non-blocking (`try_send`-class) — the runner's read loop calls this
/// inline and must never stall on a slow consumer.
///
/// This is NOT the [`GrowwOrderHintSink`] hint contract: implementers here
/// receive (and may retain) the complete event. The loud, coded `sink_drop`
/// `error!` lives in the Stage D app consumer; at THIS layer a drop is
/// count + `debug!` only.
pub trait GrowwPushEventSink: Send + Sync {
    /// Static sink name — used as the (static) metric label for drop counts.
    fn name(&self) -> &'static str;
    /// Consume one full-fidelity event, non-blocking. Return whether it was
    /// accepted; the fan-out owns the drop accounting.
    fn on_push_event(&self, event: &BrokerOrderEvent) -> PushSinkDelivery;
}

/// Fan-out container: delivers every event to EVERY registered sink,
/// non-blocking per sink, counting drops per sink
/// (`tv_groww_push_sink_dropped_total{sink}`). One slow/full sink never
/// starves the others and never blocks the runner.
pub struct GrowwPushFanOut {
    sinks: Vec<std::sync::Arc<dyn GrowwPushEventSink>>,
}

impl GrowwPushFanOut {
    /// Build a fan-out over the given sinks (Stage D wires the real set).
    #[must_use]
    pub fn new(sinks: Vec<std::sync::Arc<dyn GrowwPushEventSink>>) -> Self {
        Self { sinks }
    }

    /// Number of registered sinks (0 = deliveries are no-ops).
    #[must_use]
    pub fn sink_count(&self) -> usize {
        self.sinks.len()
    }

    /// Deliver one event to every sink. Non-blocking; a per-sink drop is
    /// counted + `debug!`-logged here (the loud coded error belongs to the
    /// Stage D consumer that owns the sink's semantics).
    pub fn deliver(&self, event: &BrokerOrderEvent) {
        for sink in &self.sinks {
            match sink.on_push_event(event) {
                PushSinkDelivery::Delivered => {}
                PushSinkDelivery::Dropped => {
                    metrics::counter!(
                        "tv_groww_push_sink_dropped_total",
                        "sink" => sink.name()
                    )
                    .increment(1);
                    tracing::debug!(
                        sink = sink.name(),
                        broker_order_id = %event.broker_order_id,
                        "groww push: full-fidelity sink refused an event (full/closed) — dropped at this sink only"
                    );
                }
            }
        }
    }
}

/// A [`GrowwPushEventSink`] over a bounded tokio mpsc queue — the standard
/// Stage D attachment shape. `try_send` only (never awaits); full or closed
/// queues report [`PushSinkDelivery::Dropped`].
#[derive(Clone)]
pub struct BoundedPushEventSink {
    name: &'static str,
    tx: tokio::sync::mpsc::Sender<BrokerOrderEvent>,
}

impl BoundedPushEventSink {
    /// Wrap an existing bounded sender as a named sink.
    #[must_use]
    pub fn new(name: &'static str, tx: tokio::sync::mpsc::Sender<BrokerOrderEvent>) -> Self {
        Self { name, tx }
    }

    /// A fresh bounded queue at [`PUSH_EVENT_CHANNEL_CAPACITY`] + the sink
    /// over its sender.
    #[must_use]
    pub fn channel(name: &'static str) -> (Self, tokio::sync::mpsc::Receiver<BrokerOrderEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(PUSH_EVENT_CHANNEL_CAPACITY);
        (Self::new(name, tx), rx)
    }
}

impl GrowwPushEventSink for BoundedPushEventSink {
    fn name(&self) -> &'static str {
        self.name
    }

    fn on_push_event(&self, event: &BrokerOrderEvent) -> PushSinkDelivery {
        match self.tx.try_send(event.clone()) {
            Ok(()) => PushSinkDelivery::Delivered,
            Err(_full_or_closed) => PushSinkDelivery::Dropped,
        }
    }
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

#[cfg(test)]
mod push_fanout_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tickvault_common::broker_order_events::BrokerOrderStatus;
    use tickvault_common::feed::Feed;

    fn sample_event() -> BrokerOrderEvent {
        BrokerOrderEvent {
            broker: Feed::Groww,
            broker_order_id: "GMK987".to_owned(),
            reference_id: None,
            status: BrokerOrderStatus::Filled,
            raw_status: "EXECUTED".to_owned(),
            filled_qty: 50,
            remaining_qty: Some(0),
            avg_fill_price_paise: Some(1_234_400),
            segment: "FNO".to_owned(),
            exchange_ts_ms: None,
            received_at_ms: 1_700_000_000_000,
        }
    }

    /// A counting sink that always accepts — proves full fidelity reaches
    /// every registered sink.
    struct RecordingSink {
        seen: Arc<AtomicUsize>,
    }

    impl GrowwPushEventSink for RecordingSink {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn on_push_event(&self, event: &BrokerOrderEvent) -> PushSinkDelivery {
            // Full fidelity: the fill/status/price fields are present here —
            // the exact fields the lossy hint lane drops.
            assert_eq!(event.status, BrokerOrderStatus::Filled);
            assert_eq!(event.filled_qty, 50);
            assert_eq!(event.avg_fill_price_paise, Some(1_234_400));
            self.seen.fetch_add(1, Ordering::SeqCst);
            PushSinkDelivery::Delivered
        }
    }

    /// A sink that always refuses — proves a dropping sink never blocks or
    /// affects its siblings.
    struct RefusingSink;

    impl GrowwPushEventSink for RefusingSink {
        fn name(&self) -> &'static str {
            "refusing"
        }
        fn on_push_event(&self, _event: &BrokerOrderEvent) -> PushSinkDelivery {
            PushSinkDelivery::Dropped
        }
    }

    #[test]
    fn test_deliver_sends_full_fidelity_events_to_every_sink() {
        let seen_a = Arc::new(AtomicUsize::new(0));
        let seen_b = Arc::new(AtomicUsize::new(0));
        let fan_out = GrowwPushFanOut::new(vec![
            Arc::new(RecordingSink {
                seen: Arc::clone(&seen_a),
            }),
            Arc::new(RecordingSink {
                seen: Arc::clone(&seen_b),
            }),
        ]);
        assert_eq!(fan_out.sink_count(), 2);
        fan_out.deliver(&sample_event());
        fan_out.deliver(&sample_event());
        assert_eq!(seen_a.load(Ordering::SeqCst), 2);
        assert_eq!(seen_b.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn one_dropping_sink_never_starves_the_others() {
        let seen = Arc::new(AtomicUsize::new(0));
        let fan_out = GrowwPushFanOut::new(vec![
            Arc::new(RefusingSink),
            Arc::new(RecordingSink {
                seen: Arc::clone(&seen),
            }),
        ]);
        fan_out.deliver(&sample_event());
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_sink_count_and_empty_fan_out_safe_no_op() {
        let fan_out = GrowwPushFanOut::new(Vec::new());
        assert_eq!(fan_out.sink_count(), 0);
        fan_out.deliver(&sample_event()); // must not panic
    }

    #[test]
    fn bounded_sink_delivers_then_drops_when_full() {
        let (sink, mut rx) = BoundedPushEventSink::channel("test_bounded");
        assert_eq!(sink.name(), "test_bounded");
        let ev = sample_event();
        // Fill to capacity — every send accepted.
        for _ in 0..PUSH_EVENT_CHANNEL_CAPACITY {
            assert_eq!(sink.on_push_event(&ev), PushSinkDelivery::Delivered);
        }
        // Capacity + 1 refuses (non-blocking, never awaits).
        assert_eq!(sink.on_push_event(&ev), PushSinkDelivery::Dropped);
        // Drain one, capacity frees up again.
        let got = rx.try_recv().unwrap_or_else(|_| panic!("one event queued"));
        assert_eq!(got.broker_order_id, "GMK987");
        assert_eq!(sink.on_push_event(&ev), PushSinkDelivery::Delivered);
    }

    #[test]
    fn bounded_sink_drops_on_a_closed_receiver() {
        let (sink, rx) = BoundedPushEventSink::channel("test_closed");
        drop(rx);
        assert_eq!(
            sink.on_push_event(&sample_event()),
            PushSinkDelivery::Dropped
        );
    }

    #[test]
    fn push_event_capacity_is_the_bounded_precedent() {
        assert_eq!(PUSH_EVENT_CHANNEL_CAPACITY, 256);
    }
}
