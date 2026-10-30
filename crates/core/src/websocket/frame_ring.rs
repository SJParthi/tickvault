//! The live frame ring: a bounded, pre-allocated queue from the socket read
//! tasks (many producers) to the frame drain (one consumer).
//!
//! # Why not `tokio::sync::mpsc`
//!
//! tokio's bounded channel stores items in a linked list of 32-slot blocks and
//! allocates a new block for every 32 queued items (`sync/mpsc/block.rs`,
//! `grow`). It keeps at most three drained blocks for reuse
//! (`list.rs::reclaim_block`) and frees the rest, so every new backlog
//! allocates again, on the socket reader task, at the moment the system is
//! already behind. `dhat_wal_ring_sink_accept` measured one block per 32
//! queued frames (audit HOT-1, 2026-10-11; finding M1 of the 2026-10-20
//! hot-path review). There is no capacity hint that pre-allocates.
//!
//! # What this is instead
//!
//! Storage is a `crossbeam_channel::bounded` array: every slot is allocated
//! once, when the ring is built, and `try_send` / `try_recv` never allocate.
//! A `tokio::sync::Notify` wakes the drain. The drain cannot poll on a timer:
//! its first `select!` arm must wake within microseconds of a frame.
//!
//! # Semantics kept from the tokio channel
//!
//! - Exact capacity: `try_send` refuses past `capacity` items (`Full`).
//! - `try_send` after the receiver is dropped refuses (`Closed`).
//! - `recv` returns `None` only after every strong sender is dropped AND every
//!   queued item has been delivered, so the drain still ends when the lane's
//!   sockets are all gone.
//! - Weak senders do not keep the ring open; `upgrade` fails once the last
//!   strong sender is gone.
//! - `recv` charges the task's cooperative budget, exactly as tokio's channel
//!   does, so a full backlog still yields to the scheduler every budget's
//!   worth of frames instead of holding its worker thread.
//! - `recv` is cancellation-safe: an item leaves the queue only in the poll
//!   that returns it.
//!
//! # Wake-up correctness
//!
//! The consumer registers interest (`Notified::enable`) BEFORE it checks the
//! queue and the sender count; a producer pushes (or drops its count) BEFORE
//! it calls `notify_one`. Both are read-modify-writes on the one `Notify`
//! state atomic, so either the notification lands on the registered waiter, or
//! it was stored as a permit that the registration consumed, and in that case
//! the push it follows is already visible to the check. Several producers'
//! notifications coalesce into one permit, which is safe because the consumer
//! always drains before it sleeps; a stale permit costs one extra loop pass.
//!
//! The consumer first takes a queued item WITHOUT registering (ITEM DT1,
//! 2026-10-30) and registers only when the queue looks empty, so the order
//! above (register, then check, then sleep) still holds for every sleep. While
//! frames are queued the drain therefore takes no waiter lock, and a producer
//! never finds it registered, so the reader pays the wake only for a drain
//! that is about to sleep.
//!
//! # Complexity
//!
//! `try_send`, `try_recv`, a ready `recv`: O(1), lock-free, zero allocation.
//! `notify_one` takes the `Notify`'s waiter lock only while the drain is
//! parked. Construction: O(capacity), once at boot.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crossbeam_channel::{TryRecvError, TrySendError};
use tokio::sync::Notify;

/// State shared by every handle of one ring.
#[derive(Debug)]
struct Shared {
    /// Wakes the consumer.
    notify: Notify,
    /// Live STRONG senders. Weak senders are not counted.
    senders: AtomicUsize,
    /// Set while the consumer busy-polls with `try_recv` and never sleeps
    /// (owner decision 2026-10-30, "Build it, test live on 2 cores": a
    /// never-sleeping drain behind a switch, default off). While set,
    /// `try_send` skips the `Notify` wake. See
    /// [`FrameRingReceiver::set_busy_polling`] for why no frame can be stranded.
    busy_polling: AtomicBool,
    /// Test-only: how many times `try_send` called `notify_one`.
    #[cfg(test)]
    notifies: AtomicUsize,
    /// Test-only: how many times the consumer called `enable()` on `notify`.
    #[cfg(test)]
    registrations: AtomicUsize,
}

/// Why [`FrameRingSender::try_send`] refused an item. The item is handed back.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameRingTrySendError<T> {
    /// The ring holds `capacity` items.
    Full(T),
    /// The receiver is gone.
    Closed(T),
}

/// Producer handle. Cloning adds a strong sender.
#[derive(Debug)]
pub struct FrameRingSender<T> {
    tx: crossbeam_channel::Sender<T>,
    shared: Arc<Shared>,
}

/// A sender that does not keep the ring open. See [`Self::upgrade`].
#[derive(Debug, Clone)]
pub struct FrameRingWeakSender<T> {
    tx: crossbeam_channel::Sender<T>,
    shared: Arc<Shared>,
}

/// The single consumer.
#[derive(Debug)]
pub struct FrameRingReceiver<T> {
    rx: crossbeam_channel::Receiver<T>,
    shared: Arc<Shared>,
}

/// Builds a ring holding at most `capacity` items, every slot allocated now.
///
/// # Panics
///
/// Never: a `capacity` of 0 is raised to 1, because a zero-capacity crossbeam
/// channel is a rendezvous channel and would refuse every `try_send`.
#[must_use]
pub fn frame_ring<T>(capacity: usize) -> (FrameRingSender<T>, FrameRingReceiver<T>) {
    let (tx, rx) = crossbeam_channel::bounded(capacity.max(1));
    let shared = Arc::new(Shared {
        notify: Notify::new(),
        senders: AtomicUsize::new(1),
        busy_polling: AtomicBool::new(false),
        #[cfg(test)]
        notifies: AtomicUsize::new(0),
        #[cfg(test)]
        registrations: AtomicUsize::new(0),
    });
    (
        FrameRingSender {
            tx,
            shared: Arc::clone(&shared),
        },
        FrameRingReceiver { rx, shared },
    )
}

impl<T> FrameRingSender<T> {
    /// Queues `item` without waiting. O(1), zero allocation.
    ///
    /// # Errors
    ///
    /// [`FrameRingTrySendError::Full`] when the ring is full,
    /// [`FrameRingTrySendError::Closed`] when the receiver is gone. Either way
    /// the item comes back.
    pub fn try_send(&self, item: T) -> Result<(), FrameRingTrySendError<T>> {
        match self.tx.try_send(item) {
            Ok(()) => {
                // SeqCst, not Relaxed: paired with the consumer's SeqCst clear
                // in `set_busy_polling(false)` (see there). A plain load on
                // x86, `ldar` on ARM; no read-modify-write either way.
                if !self.shared.busy_polling.load(Ordering::SeqCst) {
                    #[cfg(test)]
                    self.shared.notifies.fetch_add(1, Ordering::Relaxed);
                    self.shared.notify.notify_one();
                }
                Ok(())
            }
            Err(TrySendError::Full(item)) => Err(FrameRingTrySendError::Full(item)),
            Err(TrySendError::Disconnected(item)) => Err(FrameRingTrySendError::Closed(item)),
        }
    }

    /// A handle that does not keep the ring open.
    #[must_use]
    pub fn downgrade(&self) -> FrameRingWeakSender<T> {
        FrameRingWeakSender {
            // APPROVED: an Arc count bump on a handle clone, once per socket dial, never per frame.
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Clone for FrameRingSender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            // APPROVED: an Arc count bump on a handle clone, once per socket dial, never per frame.
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for FrameRingSender<T> {
    fn drop(&mut self) {
        // The last strong sender wakes the consumer so it can see the close.
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.notify.notify_one();
        }
    }
}

impl<T> FrameRingWeakSender<T> {
    /// A strong sender, or `None` once every strong sender has been dropped
    /// (the ring is closing and must not be reopened). O(1) CAS loop.
    #[must_use]
    pub fn upgrade(&self) -> Option<FrameRingSender<T>> {
        let mut current = self.shared.senders.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return None;
            }
            match self.shared.senders.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(FrameRingSender {
                        // APPROVED: an Arc count bump on a handle clone, once per socket dial, never per frame.
                        tx: self.tx.clone(),
                        shared: Arc::clone(&self.shared),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl<T> FrameRingReceiver<T> {
    /// The next item, waiting for one. `None` once every strong sender is gone
    /// and the ring is empty. Cancellation-safe; charges the cooperative budget.
    pub async fn recv(&mut self) -> Option<T> {
        tokio::task::coop::cooperative(self.recv_uncharged()).await
    }

    async fn recv_uncharged(&mut self) -> Option<T> {
        loop {
            // Fast path (ITEM DT1, 2026-10-30): take a queued item WITHOUT
            // registering. Registering costs the drain a waiter-list lock to
            // add itself and another to remove itself, and while it is
            // registered a producer's `notify_one` finds the state WAITING and
            // pays that lock plus a task wake for a drain that is already
            // running. Registration is only needed before SLEEPING, so it now
            // happens only once the queue looks empty, and the queue is
            // checked again after it (below), which is what keeps every wake.
            match self.rx.try_recv() {
                Ok(item) => return Some(item),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            // About to sleep: a busy-poll flag left set would let producers
            // skip the wake this sleep depends on. Clearing it here (the same
            // SeqCst clear as `set_busy_polling(false)`) happens before the
            // re-check below, so `recv` can never hang on it.
            if self.shared.busy_polling.load(Ordering::Relaxed) {
                self.set_busy_polling(false);
            }
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            // Register BEFORE checking, so a push that lands after the check
            // wakes this waiter (see the module docs).
            #[cfg(test)]
            self.shared.registrations.fetch_add(1, Ordering::Relaxed);
            notified.as_mut().enable();
            match self.rx.try_recv() {
                Ok(item) => return Some(item),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            if self.shared.senders.load(Ordering::Acquire) == 0 {
                // Closed: deliver anything pushed just before the last drop.
                return self.rx.try_recv().ok();
            }
            notified.await;
        }
    }

    /// Declares that the consumer busy-polls with [`Self::try_recv`] and
    /// never sleeps (`true`), or that it may sleep in [`Self::recv`] again
    /// (`false`). Default `false`, which is the behaviour before this flag.
    ///
    /// While set, `try_send` skips the `Notify` wake: the reader pays no
    /// waiter check and no cross-thread wake for a drain that is not asleep.
    ///
    /// # Why clearing it strands no frame
    ///
    /// A producer pushes (crossbeam's tail compare-and-swap, `SeqCst`), then
    /// loads this flag (`SeqCst`). Clearing it is a `SeqCst` store, and the
    /// consumer's next `try_recv` declares the ring empty only after a `SeqCst`
    /// fence and a tail load (crossbeam-channel 0.5.17, `array.rs`
    /// `start_recv`, pinned). If a producer still read the flag as set, its
    /// load comes before the clear in the single total order of `SeqCst`
    /// operations, so its tail update comes before that fence, and the tail
    /// load sees the frame: the consumer takes it instead of sleeping. A
    /// producer that reads the flag as clear wakes the consumer as before.
    /// `recv` also clears the flag itself before it registers to sleep.
    ///
    /// O(1): one `SeqCst` store.
    // WIRING-EXEMPT: called by the busy-poll drain (owner 2026-10-30, "Build it, test live on 2 cores"), built behind a default-off switch by the core-placement session; until then the flag stays false and nothing changes.
    pub fn set_busy_polling(&self, on: bool) {
        self.shared.busy_polling.store(on, Ordering::SeqCst);
    }

    /// Whether the consumer has declared it busy-polls. One load.
    #[must_use]
    pub fn is_busy_polling(&self) -> bool {
        self.shared.busy_polling.load(Ordering::Relaxed)
    }

    /// The next item if one is queued now. O(1).
    pub fn try_recv(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }

    /// Items queued now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rx.len()
    }

    /// Whether nothing is queued now.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::time::Duration;

    #[tokio::test]
    async fn test_frame_ring_delivers_in_order_then_none_after_the_last_sender() {
        let (tx, mut rx) = frame_ring::<u32>(8);
        for i in 0..5 {
            tx.try_send(i).expect("room");
        }
        drop(tx);
        for i in 0..5 {
            assert_eq!(rx.recv().await, Some(i), "queued items survive the close");
        }
        assert_eq!(rx.recv().await, None);
    }

    #[test]
    fn test_frame_ring_try_send_refuses_past_capacity_and_hands_the_item_back() {
        let (tx, mut rx) = frame_ring::<u32>(2);
        tx.try_send(1).expect("room");
        tx.try_send(2).expect("room");
        assert_eq!(tx.try_send(3), Err(FrameRingTrySendError::Full(3)));
        assert_eq!(rx.len(), 2);
        assert_eq!(rx.try_recv(), Some(1));
        tx.try_send(3).expect("room again");
        assert!(!rx.is_empty());
    }

    #[test]
    fn test_frame_ring_try_send_after_the_receiver_is_dropped_is_closed() {
        let (tx, rx) = frame_ring::<u32>(2);
        drop(rx);
        assert_eq!(tx.try_send(7), Err(FrameRingTrySendError::Closed(7)));
    }

    #[test]
    fn test_frame_ring_zero_capacity_is_raised_to_one() {
        let (tx, _rx) = frame_ring::<u32>(0);
        tx.try_send(1).expect("one slot");
        assert_eq!(tx.try_send(2), Err(FrameRingTrySendError::Full(2)));
    }

    #[test]
    fn test_frame_ring_try_recv_len_and_is_empty_track_the_queue() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        assert!(rx.is_empty());
        assert_eq!(rx.len(), 0);
        tx.try_send(1).expect("room");
        tx.try_send(2).expect("room");
        assert_eq!(rx.len(), 2);
        assert!(!rx.is_empty());
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(rx.try_recv(), None);
        assert!(rx.is_empty());
    }

    #[test]
    fn test_frame_ring_upgrade_succeeds_while_a_strong_sender_lives() {
        let (tx, _rx) = frame_ring::<u32>(4);
        let weak = tx.downgrade();
        let a = weak.upgrade().expect("open");
        let b = weak.upgrade().expect("open");
        drop(tx);
        drop(a);
        assert!(weak.upgrade().is_some(), "b still holds it open");
        drop(b);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn test_frame_ring_downgrade_gives_a_weak_sender_that_does_not_keep_the_ring_open() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        let weak = tx.downgrade();
        let strong = weak.upgrade().expect("a strong sender is alive");
        drop(tx);
        strong.try_send(1).expect("room");
        drop(strong);
        assert!(weak.upgrade().is_none(), "never reopens a closing ring");
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, None, "the weak sender holds nothing open");
    }

    #[tokio::test]
    async fn test_frame_ring_a_cloned_sender_keeps_the_ring_open() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        let second = tx.clone();
        drop(tx);
        second.try_send(9).expect("room");
        assert_eq!(rx.recv().await, Some(9));
        let pending = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(pending.is_err(), "still open while a clone lives");
        drop(second);
        assert_eq!(rx.recv().await, None);
    }

    /// The wake path across threads, many times: the drain parks, a producer
    /// on another thread pushes, the drain must wake and get it every time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_frame_ring_cross_thread_wake_never_loses_an_item() {
        const PRODUCERS: u64 = 4;
        const PER_PRODUCER: u64 = 10_000;
        let (tx, mut rx) = frame_ring::<u64>(64);
        let mut handles = Vec::new();
        for p in 0..PRODUCERS {
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                let mut sent = 0;
                while sent < PER_PRODUCER {
                    let v = p * PER_PRODUCER + sent;
                    match tx.try_send(v) {
                        Ok(()) => sent += 1,
                        Err(FrameRingTrySendError::Full(_)) => std::thread::yield_now(),
                        Err(FrameRingTrySendError::Closed(_)) => panic!("receiver alive"),
                    }
                }
            }));
        }
        drop(tx);
        let mut seen = vec![0u64; PRODUCERS as usize];
        let mut total = 0;
        let mut sum: u64 = 0;
        let got = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(v) = rx.recv().await {
                seen[(v / PER_PRODUCER) as usize] += 1;
                sum = sum.wrapping_add(v);
                total += 1;
            }
        })
        .await;
        assert!(got.is_ok(), "the drain must end: a lost wake hangs here");
        for h in handles {
            h.join().expect("producer");
        }
        assert_eq!(total, PRODUCERS * PER_PRODUCER);
        assert!(seen.iter().all(|&n| n == PER_PRODUCER));
        let n = PRODUCERS * PER_PRODUCER;
        assert_eq!(sum, n * (n - 1) / 2, "every value exactly once");
    }

    /// A biased select with an always-ready rival arm, as in the drain: the
    /// recv future is dropped over and over and must never eat an item.
    #[tokio::test]
    async fn test_frame_ring_recv_dropped_inside_select_loses_nothing() {
        let (tx, mut rx) = frame_ring::<u32>(1_024);
        for i in 0..1_000 {
            tx.try_send(i).expect("room");
        }
        drop(tx);
        let mut got = Vec::new();
        let mut rival_wins = 0;
        loop {
            tokio::select! {
                biased;
                _ = std::future::ready(()), if rival_wins % 3 == 0 => {
                    rival_wins += 1;
                }
                item = rx.recv() => match item {
                    Some(v) => { got.push(v); rival_wins += 1; }
                    None => break,
                },
            }
        }
        assert_eq!(got, (0..1_000).collect::<Vec<_>>());
    }

    /// A waker that counts how many times it was woken.
    struct CountingWaker(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// ITEM DT1 (2026-10-30): while frames are queued the drain never touches
    /// the `Notify`, so it takes no waiter lock, and a producer pushing at the
    /// same time never finds it registered and never pays a waiter lock and a
    /// task wake for a drain that is already running.
    ///
    /// Bite: before the fast path every `recv` called `enable()` first. With a
    /// stored permit that consumed it; without one (every pop once the
    /// producer below has finished) it took the waiter lock to register and
    /// again to deregister on return, and a push landing in that window woke
    /// this waker. `registrations` counts the `enable()` calls: the old `recv`
    /// makes one per pop (300,000), the new one makes none while an item is
    /// queued. Then, once the queue IS empty, the drain parks and the next push
    /// wakes it exactly once.
    #[test]
    fn test_reader_wakes_the_drain_only_when_it_is_parked() {
        const PREFILL: u64 = 400_000;
        const CONCURRENT: u64 = 200_000;
        const POPS: u64 = 300_000;
        let (tx, mut rx) = frame_ring::<u64>(1 << 20);
        for i in 0..PREFILL {
            tx.try_send(i).expect("room");
        }
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);
        let producer = {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for i in 0..CONCURRENT {
                    tx.try_send(PREFILL + i).expect("room");
                }
            })
        };
        for expected in 0..POPS {
            let mut fut = std::pin::pin!(rx.recv());
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(Some(v)) => assert_eq!(v, expected, "in order"),
                other => panic!("an item was queued, got {other:?}"),
            }
        }
        producer.join().expect("producer");
        assert_eq!(
            rx.shared.registrations.load(Ordering::Relaxed),
            0,
            "the drain registered on the Notify although frames were queued"
        );
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "a producer woke a drain that had frames queued"
        );
        // Drain the rest, then the drain parks: the next push wakes it once.
        while rx.try_recv().is_some() {}
        let mut fut = std::pin::pin!(rx.recv());
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "empty: the drain parks"
        );
        tx.try_send(7).expect("room");
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "a parked drain is woken"
        );
        tx.try_send(8).expect("room");
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "wakes coalesce");
        assert_eq!(fut.as_mut().poll(&mut cx), std::task::Poll::Ready(Some(7)));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// ITEM DT1: with the fast path, no interleaving of producers and a
        /// parking drain loses a wake: every item is received and the drain
        /// ends. Producers push with random pauses so the drain parks and
        /// wakes many times per case.
        #[test]
        fn proptest_frame_ring_sleeping_consumer_never_loses_a_wake(
            producers in 1_u64..5,
            per_producer in 1_u64..3_000,
            pause_every in 1_u64..64,
            pause_spins in 0_u32..20_000,
            capacity in 1_usize..256,
        ) {
            // The drain runs on this thread (`block_on`); the producers are
            // their own OS threads.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime");
            let (tx, mut rx) = frame_ring::<u64>(capacity);
            let mut handles = Vec::new();
            for p in 0..producers {
                let tx = tx.clone();
                handles.push(std::thread::spawn(move || {
                    let mut sent = 0;
                    while sent < per_producer {
                        match tx.try_send(p * per_producer + sent) {
                            Ok(()) => {
                                sent += 1;
                                if sent % pause_every == 0 {
                                    for _ in 0..pause_spins {
                                        std::hint::spin_loop();
                                    }
                                }
                            }
                            Err(FrameRingTrySendError::Full(_)) => std::thread::yield_now(),
                            Err(FrameRingTrySendError::Closed(_)) => panic!("receiver alive"),
                        }
                    }
                }));
            }
            drop(tx);
            let total = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(30), async {
                    let mut total = 0_u64;
                    while rx.recv().await.is_some() {
                        total += 1;
                    }
                    total
                })
                .await
            });
            for h in handles {
                h.join().expect("producer");
            }
            proptest::prop_assert_eq!(total, Ok(producers * per_producer), "a lost wake hangs or drops");
        }
    }

    /// Busy-poll flag (owner 2026-10-30): default off, so every push notifies
    /// as before; while set, pushes skip the `Notify`; cleared, they notify
    /// again. Nothing is lost either way.
    #[test]
    fn test_try_send_skips_the_wake_only_while_the_drain_busy_polls() {
        let (tx, mut rx) = frame_ring::<u32>(16);
        assert!(!rx.is_busy_polling(), "default off: today's behaviour");
        tx.try_send(1).expect("room");
        assert_eq!(rx.shared.notifies.load(Ordering::Relaxed), 1);
        rx.set_busy_polling(true);
        assert!(rx.is_busy_polling());
        for i in 2..=4 {
            tx.try_send(i).expect("room");
        }
        assert_eq!(
            rx.shared.notifies.load(Ordering::Relaxed),
            1,
            "a busy-polling drain is not notified"
        );
        rx.set_busy_polling(false);
        tx.try_send(5).expect("room");
        assert_eq!(rx.shared.notifies.load(Ordering::Relaxed), 2);
        let got: Vec<u32> = std::iter::from_fn(|| rx.try_recv()).collect();
        assert_eq!(got, vec![1, 2, 3, 4, 5], "every frame, in order");
    }

    /// `recv` left with the flag set cannot hang: it clears the flag before
    /// it registers to sleep, so the next push wakes it.
    #[test]
    fn test_recv_clears_busy_polling_before_it_parks() {
        let (tx, mut rx) = frame_ring::<u32>(16);
        rx.set_busy_polling(true);
        let shared = Arc::clone(&rx.shared);
        let busy_flag = &shared.busy_polling;
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = std::pin::pin!(rx.recv());
        assert!(fut.as_mut().poll(&mut cx).is_pending(), "empty: parks");
        tx.try_send(9).expect("room");
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the parked drain is woken"
        );
        assert_eq!(fut.as_mut().poll(&mut cx), std::task::Poll::Ready(Some(9)));
        assert!(!busy_flag.load(Ordering::Relaxed), "recv cleared the flag");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// The consumer switches between busy-polling (`try_recv` spin, flag
        /// set) and sleeping in `recv` (flag cleared) at random while several
        /// producers push with random pauses. A frame stranded by a skipped
        /// wake would hang the sleeping phase into the timeout.
        #[test]
        fn proptest_busy_poll_switches_never_strand_a_frame(
            producers in 1_u64..5,
            per_producer in 1_u64..3_000,
            pause_every in 1_u64..64,
            pause_spins in 0_u32..20_000,
            phase_len in 1_u64..200,
            capacity in 1_usize..256,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime");
            let (tx, mut rx) = frame_ring::<u64>(capacity);
            let mut handles = Vec::new();
            for p in 0..producers {
                let tx = tx.clone();
                handles.push(std::thread::spawn(move || {
                    let mut sent = 0;
                    while sent < per_producer {
                        match tx.try_send(p * per_producer + sent) {
                            Ok(()) => {
                                sent += 1;
                                if sent % pause_every == 0 {
                                    for _ in 0..pause_spins {
                                        std::hint::spin_loop();
                                    }
                                }
                            }
                            Err(FrameRingTrySendError::Full(_)) => std::thread::yield_now(),
                            Err(FrameRingTrySendError::Closed(_)) => panic!("receiver alive"),
                        }
                    }
                }));
            }
            drop(tx);
            let total = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(30), async {
                    let mut total = 0_u64;
                    let mut busy = false;
                    let mut in_phase = 0_u64;
                    loop {
                        in_phase += 1;
                        if in_phase >= phase_len {
                            in_phase = 0;
                            busy = !busy;
                            rx.set_busy_polling(busy);
                        }
                        if busy {
                            if rx.try_recv().is_some() {
                                total += 1;
                            } else if rx.shared.senders.load(Ordering::Acquire) == 0
                                && rx.is_empty()
                            {
                                break;
                            } else {
                                std::hint::spin_loop();
                            }
                        } else {
                            rx.set_busy_polling(false);
                            match rx.recv().await {
                                Some(_) => total += 1,
                                None => break,
                            }
                        }
                    }
                    total
                })
                .await
            });
            for h in handles {
                h.join().expect("producer");
            }
            proptest::prop_assert_eq!(total, Ok(producers * per_producer), "a stranded frame hangs or drops");
        }
    }

    /// A full backlog must not hold a current-thread runtime: the cooperative
    /// budget makes `recv` yield so another task gets to run.
    #[tokio::test(flavor = "current_thread")]
    async fn test_frame_ring_a_full_backlog_yields_to_other_tasks() {
        let (tx, mut rx) = frame_ring::<u32>(100_000);
        for i in 0..100_000 {
            tx.try_send(i).expect("room");
        }
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let other = Arc::clone(&flag);
        tokio::spawn(async move { other.store(true, Ordering::SeqCst) });
        let mut drained_before_other_ran = None;
        for n in 0..100_000 {
            let _ = rx.recv().await;
            if flag.load(Ordering::SeqCst) {
                drained_before_other_ran = Some(n);
                break;
            }
        }
        let n = drained_before_other_ran.expect("the other task must run before the backlog ends");
        assert!(n < 10_000, "yielded only after {n} items");
        drop(tx);
    }
}
