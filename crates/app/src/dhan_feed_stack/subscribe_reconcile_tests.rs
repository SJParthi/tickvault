//! Cold-path registration and acknowledgement ownership for plan item 3b.
//! These fixtures use channels and an in-memory writer; they never dial a feed.

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::subscribe_reconcile::{
        RECONCILE_DELTA_SECS, RECONCILE_MISSING_GAUGE, SubscribeReconcile,
    };
    use std::collections::HashSet;

    fn instrument(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    #[test]
    fn main_feed_commands_registers_without_a_topup_collector() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let guard = SubscribeGuard::try_new(
            DhanEndpointType::MainFeed,
            vec![instrument(1), instrument(2)],
        )
        .expect("valid main-feed set");
        let receiver = main_feed_commands(DhanEndpointType::MainFeed, &guard, None, Some(&book))
            .expect("the observer owns a sender even without a top-up collector");
        assert_eq!(book.expected_len(), 2);
        assert_eq!(receiver.capacity(), 1, "command queue stays bounded");
    }

    #[test]
    fn main_feed_commands_preserves_topup_room_and_excludes_depth() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, vec![instrument(1)])
            .expect("valid main-feed set");
        let mut topups = Vec::new();
        let _receiver = main_feed_commands(
            DhanEndpointType::MainFeed,
            &guard,
            Some(&mut topups),
            Some(&book),
        );
        assert_eq!(topups.len(), 1);
        assert_eq!(topups[0].1, guard.spare_capacity());
        let depth = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![instrument(2)])
            .expect("valid depth set");
        assert!(
            main_feed_commands(
                DhanEndpointType::Depth200,
                &depth,
                Some(&mut topups),
                Some(&book)
            )
            .is_none()
        );
        assert_eq!(
            topups.len(),
            1,
            "depth retains its independent channel path"
        );
        assert_eq!(
            book.expected_len(),
            1,
            "depth cannot enter the main-feed book"
        );
    }

    fn queue_two(
        book: &Arc<SubscribeReconcile>,
    ) -> (
        Vec<PendingTopUp>,
        HashSet<(u64, u8)>,
        tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>,
    ) {
        let (tx, receiver) = tokio::sync::mpsc::channel(1);
        let mut slots = vec![(tx, 2)];
        let mut pending = Vec::new();
        let mut sent = HashSet::new();
        let placed = send_extend_chunks(
            &[instrument(1), instrument(2)],
            &mut sent,
            &mut slots,
            &mut pending,
            1,
            Some(book),
        );
        assert_eq!(placed, 2);
        assert_eq!(book.expected_len(), 2);
        assert_eq!(pending.len(), 1);
        assert_eq!(slots[0].1, 0);
        (pending, sent, receiver)
    }

    fn take_ack(
        receiver: &mut tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>,
    ) -> tokio::sync::oneshot::Sender<ExtendOutcome> {
        let LiveSubscriptionCommand::Extend {
            more,
            ack: Some(ack),
        } = receiver.try_recv().expect("queued offer")
        else {
            panic!("a main-feed top-up must carry its Extend acknowledgement");
        };
        assert_eq!(more, vec![instrument(1), instrument(2)]);
        ack
    }

    #[test]
    fn a_held_topup_remains_expected_after_its_ack() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (mut pending, mut sent, mut receiver) = queue_two(&book);
        assert_eq!(reconcile_pending_topups(&mut pending, &mut sent, 2), 0);
        assert_eq!(
            pending.len(),
            1,
            "an unanswered queued command remains pending"
        );
        take_ack(&mut receiver)
            .send(ExtendOutcome::Held)
            .expect("receiver held");
        assert_eq!(reconcile_pending_topups(&mut pending, &mut sent, 3), 0);
        assert!(pending.is_empty());
        assert_eq!(book.expected_len(), 2);
        assert_eq!(sent.len(), 2);
    }

    #[test]
    fn a_refused_topup_is_removed_from_both_expected_sets() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (mut pending, mut sent, mut receiver) = queue_two(&book);
        take_ack(&mut receiver)
            .send(ExtendOutcome::Refused)
            .expect("receiver held");
        assert_eq!(reconcile_pending_topups(&mut pending, &mut sent, 2), 2);
        assert!(pending.is_empty());
        assert!(sent.is_empty());
        assert_eq!(book.expected_len(), 0);
    }

    #[test]
    fn a_truncated_topup_removes_only_the_unheld_tail() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (mut pending, mut sent, mut receiver) = queue_two(&book);
        take_ack(&mut receiver)
            .send(ExtendOutcome::Truncated {
                not_held: vec![instrument(2)],
            })
            .expect("receiver held");
        assert_eq!(reconcile_pending_topups(&mut pending, &mut sent, 2), 1);
        assert!(pending.is_empty());
        assert_eq!(sent, HashSet::from([contract_identity(&instrument(1))]));
        assert_eq!(book.expected_len(), 1);
    }

    #[test]
    fn a_closed_ack_removes_the_unconfirmed_offer() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (mut pending, mut sent, mut receiver) = queue_two(&book);
        drop(take_ack(&mut receiver));
        assert_eq!(reconcile_pending_topups(&mut pending, &mut sent, 2), 2);
        assert!(pending.is_empty());
        assert!(sent.is_empty());
        assert_eq!(book.expected_len(), 0);
    }

    #[test]
    fn refused_command_queues_never_record_an_expected_offer() {
        for closed in [false, true] {
            let (book, _rounds) = SubscribeReconcile::new(false);
            let (tx, mut receiver) = tokio::sync::mpsc::channel(1);
            if closed {
                receiver.close();
            } else {
                tx.try_send(LiveSubscriptionCommand::Extend {
                    more: vec![instrument(9)],
                    ack: None,
                })
                .expect("fill the sole queue slot");
            }
            let mut sent = HashSet::new();
            let mut pending = Vec::new();
            let mut slots = vec![(tx, 2)];
            assert_eq!(
                send_extend_chunks(
                    &[instrument(1)],
                    &mut sent,
                    &mut slots,
                    &mut pending,
                    1,
                    Some(&book)
                ),
                0
            );
            assert!(sent.is_empty());
            assert!(pending.is_empty());
            assert_eq!(slots[0].1, 2);
            assert_eq!(book.expected_len(), 0);
        }
    }

    #[test]
    fn real_silence_sweep_observes_sparse_expected_reports_before_alarm_filtering() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let (book, mut rounds) = SubscribeReconcile::new(false);
            let (tx, _receiver) = tokio::sync::mpsc::channel(1);
            book.record_subscribed(&tx, &[instrument(1)]);
            let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 2);
            ingest.detector = TickGapDetector::with_capacity(
                2,
                DetectorConfig {
                    sparse_baseline_millis: 0,
                    ..DetectorConfig::default()
                },
            );
            ingest.subscribe_reconcile = Some(book);
            assert!(ingest.seed(1, ExchangeSegment::NseFno, 0));
            assert!(ingest.seed(2, ExchangeSegment::NseFno, 0));
            let mut worst = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
            assert_eq!(
                ingest.scan_silence_named_at(
                    IN_SESSION_1000_IST_MILLIS,
                    &mut worst,
                    Instant::now() + std::time::Duration::from_secs(RECONCILE_DELTA_SECS + 1),
                ),
                (0, 0, 0),
                "sparse instruments are excluded from the existing alarm"
            );
            assert!(
                rounds.try_recv().is_err(),
                "default shadow mode cannot send a round"
            );
        });
        let sample = format!("{RECONCILE_MISSING_GAUGE} 1");
        let rendered = handle.render();
        assert!(
            rendered.lines().any(|line| line == sample),
            "one expected sparse instrument must reach reconciliation; the untracked instrument must not: {rendered}"
        );
    }
}
