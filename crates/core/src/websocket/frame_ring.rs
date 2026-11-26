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
//! Storage is a closeable `ConcurrentQueue::bounded` array: every slot is allocated
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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use tokio::sync::Notify;

/// The longest spin [`FrameRingReceiver::set_spin_before_park`] accepts.
/// The spin holds the consumer's thread, so a config typo must not turn it
/// into a stall: 1 ms caps it well under any drain stage budget.
pub const FRAME_RING_SPIN_MAX: Duration = Duration::from_micros(FRAME_RING_SPIN_MAX_MICROS);

/// [`FRAME_RING_SPIN_MAX`] in microseconds.
pub const FRAME_RING_SPIN_MAX_MICROS: u64 = 1_000;

/// Spins in a row that may catch a frame before the ring takes one ordinary
/// sleep. Without it a rush of frames closer than the spin keeps `recv`
/// returning ready, and a `select!` that lists the ring first never polls its
/// timer or idle arms until the rush ends (US1-B review, 2026-11-24).
pub const FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS: u32 = 64;

/// Ring checks between two clock reads while spinning. A clock read costs
/// about 20 to 30 ns and an empty check a few ns, so the spin can overrun
/// its budget by at most this many checks (Assumed well under 1 µs; the
/// measured overrun is `spin_nanos - misses x budget`).
const FRAME_RING_SPIN_CHECKS_PER_CLOCK_READ: u32 = 32;

/// State shared by every handle of one ring.
#[derive(Debug)]
struct Shared<T> {
    queue: ConcurrentQueue<(usize, T)>,
    // Resolved once per class when a sink is wired. Only cancellation reads the callbacks.
    discard: Mutex<Vec<DiscardHandler<T>>>,
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

struct DiscardHandler<T> {
    key: (usize, u8),
    // APPROVED: only receiver teardown invokes this type-erased callback. Queued items carry its integer tag.
    handle: Box<dyn Fn(&T) + Send + Sync>,
}

impl<T> std::fmt::Debug for DiscardHandler<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscardHandler").finish_non_exhaustive()
    }
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
    discard_tag: usize,
    shared: Arc<Shared<T>>,
}

/// A sender that does not keep the ring open. See [`Self::upgrade`].
#[derive(Debug, Clone)]
pub struct FrameRingWeakSender<T> {
    discard_tag: usize,
    shared: Arc<Shared<T>>,
}

/// The single consumer.
#[derive(Debug)]
pub struct FrameRingReceiver<T> {
    shared: Arc<Shared<T>>,
    /// How long `recv` keeps checking an empty ring before it sleeps. Zero
    /// (the default) sleeps at once, the behaviour before this field.
    spin_before_park: Duration,
    /// Spins that ended with an item, spins that ran out, and time spent
    /// spinning (ns). Plain integers: the receiver has one owner.
    spin_stats: FrameRingSpinStats,
    /// Whether the next empty ring is worth a spin: set after a spin that
    /// caught a frame or a sleep shorter than the spin, cleared after a spin
    /// that ran out (adaptive spinning, US1-B).
    spin_armed: bool,
    /// Spins in a row that caught a frame. At
    /// [`FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS`] the spin disarms so the next
    /// empty ring sleeps once and the caller's other `select!` arms are polled.
    spin_hit_streak: u32,
}

/// What the short wait before sleeping did, since the ring was built. Read
/// by the drain and published as counters (US1-B).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameRingSpinStats {
    /// Spins that took a frame (no wake on either side).
    pub hits: u64,
    /// Spins that found nothing and fell through to sleep.
    pub misses: u64,
    /// Nanoseconds spent spinning, hits and misses together: the CPU cost.
    pub spin_nanos: u64,
}

/// Builds a ring holding at most `capacity` items, every slot allocated now.
///
/// # Panics
///
/// A capacity of zero is raised to one, preserving a non-rendezvous ring.
#[must_use]
pub fn frame_ring<T>(capacity: usize) -> (FrameRingSender<T>, FrameRingReceiver<T>) {
    let shared = Arc::new(Shared {
        queue: ConcurrentQueue::bounded(capacity.max(1)),
        // APPROVED: ring construction only. An empty Vec allocates nothing on rings without a discard handler.
        discard: Mutex::new(Vec::new()),
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
            discard_tag: 0,
            shared: Arc::clone(&shared),
        },
        FrameRingReceiver {
            shared,
            spin_before_park: Duration::ZERO,
            spin_stats: FrameRingSpinStats::default(),
            spin_armed: true,
            spin_hit_streak: 0,
        },
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
        match self.shared.queue.push((self.discard_tag, item)) {
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
            Err(PushError::Full((_, item))) => Err(FrameRingTrySendError::Full(item)),
            Err(PushError::Closed((_, item))) => Err(FrameRingTrySendError::Closed(item)),
        }
    }

    /// Associates this sender's queued items with a cold-path refund. The
    /// handler holds the key's budget alive, so its address cannot be reused.
    pub(crate) fn with_discard_handler(
        mut self,
        key: (usize, u8),
        handler: impl Fn(&T) + Send + Sync + 'static,
    ) -> Self {
        let mut handlers = self
            .shared
            .discard
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let index = handlers
            .iter()
            .position(|h| h.key == key)
            .unwrap_or_else(|| {
                handlers.push(DiscardHandler {
                    key,
                    // APPROVED: allocate once when wiring a new budget/transport pair. Reconnecting senders reuse the existing tag.
                    handle: Box::new(handler),
                });
                handlers.len() - 1
            });
        self.discard_tag = index + 1;
        drop(handlers);
        self
    }

    pub(crate) fn receiver_closed(&self) -> bool {
        self.shared.queue.is_closed()
    }

    /// A handle that does not keep the ring open.
    #[must_use]
    pub fn downgrade(&self) -> FrameRingWeakSender<T> {
        FrameRingWeakSender {
            // APPROVED: an Arc count bump on a handle clone, once per socket dial, never per frame.
            discard_tag: self.discard_tag,
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Clone for FrameRingSender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            // APPROVED: an Arc count bump on a handle clone, once per socket dial, never per frame.
            discard_tag: self.discard_tag,
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for FrameRingSender<T> {
    fn drop(&mut self) {
        // The last strong sender wakes the consumer so it can see the close.
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.queue.close();
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
                        discard_tag: self.discard_tag,
                        shared: Arc::clone(&self.shared),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl<T> Drop for FrameRingReceiver<T> {
    fn drop(&mut self) {
        self.close();
        let handlers = std::mem::take(
            &mut *self
                .shared
                .discard
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        while let Ok((tag, item)) = self.shared.queue.pop() {
            if tag != 0 {
                (handlers[tag - 1].handle)(&item);
            }
        }
    }
}

impl<T> FrameRingReceiver<T> {
    /// Stops admissions before draining the final backlog. In-flight pushes
    /// either preceded the close and are drainable, or return their item.
    pub fn close(&self) {
        self.shared.queue.close();
    }

    fn pop_value(&self) -> Result<T, PopError> {
        self.shared.queue.pop().map(|(_, item)| item)
    }

    /// The next item, waiting for one. `None` once every strong sender is gone
    /// and the ring is empty. Cancellation-safe; charges the cooperative budget.
    pub async fn recv(&mut self) -> Option<T> {
        tokio::task::coop::cooperative(self.recv_uncharged()).await
    }

    async fn recv_uncharged(&mut self) -> Option<T> {
        // Set by a sleep that began with no spin before it in the same pass
        // and ended within the spin budget; consumed by the next check. Only
        // a wake that then finds a frame re-arms the spin (US1-B review).
        let mut quick_wake = false;
        loop {
            // Fast path (ITEM DT1, 2026-10-30): take a queued item WITHOUT
            // registering. Registering costs the drain a waiter-list lock to
            // add itself and another to remove itself, and while it is
            // registered a producer's `notify_one` finds the state WAITING and
            // pays that lock plus a task wake for a drain that is already
            // running. Registration is only needed before SLEEPING, so it now
            // happens only once the queue looks empty, and the queue is
            // checked again after it (below), which is what keeps every wake.
            match self.pop_value() {
                Ok(item) => {
                    if quick_wake {
                        self.spin_armed = true;
                    }
                    return Some(item);
                }
                Err(PopError::Closed) => return None,
                Err(PopError::Empty) => {}
            }
            // A wake that found nothing (a stale `Notify` permit) re-arms
            // nothing.
            quick_wake = false;
            // Short wait before sleeping (plan item US1-B, 2026-11-24): keep
            // checking for up to `spin_before_park` with the busy-poll flag
            // set, so a frame that lands in that time is taken with no wake
            // on either side (the reader skips `notify_one`, the drain is not
            // rescheduled). Off when zero. Ending the spin falls through to
            // the unchanged clear, register and re-check below, so no frame
            // pushed during the spin is stranded (see `set_busy_polling`).
            // Adaptive: a spin that runs out disarms it. It is re-armed only by
            // a later sleep with no spin before it that a frame ended within
            // the budget (below), so a slow feed pays one spin per lull, not
            // one per frame. A sleep right after a missed spin starts one
            // budget late, so its length says nothing about the gap and never
            // re-arms (US1-B review: gaps of one to two budgets re-armed it).
            let mut spun = false;
            if self.spin_armed && !self.spin_before_park.is_zero() {
                spun = true;
                let caught = self.spin_for_next();
                self.note_spin_outcome(caught.is_some());
                if let Some(item) = caught {
                    return item;
                }
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
            match self.pop_value() {
                Ok(item) => return Some(item),
                Err(PopError::Closed) => return None,
                Err(PopError::Empty) => {}
            }
            if self.shared.senders.load(Ordering::Acquire) == 0 {
                // Closed: deliver anything pushed just before the last drop.
                return self.pop_value().ok();
            }
            if self.spin_before_park.is_zero() {
                notified.await;
            } else {
                // Two clock reads per sleep, spin on only. The measured time
                // includes the wake itself, so it under-reports re-arms
                // (keeps the spin off), never over-reports.
                let parked_at = Instant::now();
                notified.await;
                quick_wake = !spun && parked_at.elapsed() < self.spin_before_park;
            }
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
    /// A producer pushes (the array's tail compare-and-swap, `SeqCst`), then
    /// loads this flag (`SeqCst`). Clearing it is a `SeqCst` store, and the
    /// consumer's next `try_recv` declares the ring empty only after a `SeqCst`
    /// fence and a tail load (`concurrent-queue` 2.5.0, `bounded.rs::pop`).
    /// The one-slot specialization checks its state with a SeqCst CAS. If a
    /// producer still read the flag as set, its
    /// load comes before the clear in the single total order of `SeqCst`
    /// operations, so its tail update comes before that fence, and the tail
    /// load sees the frame: the consumer takes it instead of sleeping. A
    /// producer that reads the flag as clear wakes the consumer as before.
    /// `recv` also clears the flag itself before it registers to sleep.
    ///
    /// O(1): one `SeqCst` store.
    // WIRING-EXEMPT: called by the busy-poll drain (owner 2026-10-30, "Build it, test live on 2 cores"), built behind a default-off switch by the core-placement session; since 2026-11-24 (US1-B) `recv`'s short wait also sets the flag through `spin_for_next`.
    pub fn set_busy_polling(&self, on: bool) {
        self.shared.busy_polling.store(on, Ordering::SeqCst);
    }

    /// Sets how long [`Self::recv`] keeps checking an empty ring before it
    /// sleeps, capped at [`FRAME_RING_SPIN_MAX`]. Zero turns it off.
    ///
    /// The spin runs on the consumer's own thread and holds it: a tokio task
    /// calling `recv` does not yield for up to this long when nothing
    /// arrives. Cost: up to `min(gap, spin)` of CPU per idle gap.
    // WIRING-EXEMPT: called once by the live lane at ring construction (`run_dhan_feed_stack`), from `[dhan_feed] frame_drain_spin_us`.
    pub fn set_spin_before_park(&mut self, spin: Duration) {
        self.spin_before_park = spin.min(FRAME_RING_SPIN_MAX);
    }

    /// The spin set by [`Self::set_spin_before_park`].
    #[must_use]
    pub fn spin_before_park(&self) -> Duration {
        self.spin_before_park
    }

    /// Checks the ring until an item, a disconnect, or the spin budget ends.
    /// `Some(Some(item))`: an item; `Some(None)`: disconnected; `None`: the
    /// budget ran out with the ring empty. Sets the busy-poll flag for the
    /// spin and leaves it set: the caller's sleep path clears it before it
    /// registers, and a caller that returns an item keeps it, so the next
    /// frame's reader also skips the wake. O(spin / check), no allocation.
    fn spin_for_next(&mut self) -> Option<Option<T>> {
        if !self.shared.busy_polling.load(Ordering::Relaxed) {
            self.set_busy_polling(true);
        }
        let started = Instant::now();
        let deadline = started + self.spin_before_park;
        loop {
            for _ in 0..FRAME_RING_SPIN_CHECKS_PER_CLOCK_READ {
                match self.pop_value() {
                    Ok(item) => {
                        self.note_spin(started, Instant::now(), true);
                        return Some(Some(item));
                    }
                    Err(PopError::Closed) => return Some(None),
                    Err(PopError::Empty) => std::hint::spin_loop(),
                }
            }
            let now = Instant::now();
            if now >= deadline {
                self.note_spin(started, now, false);
                return None;
            }
        }
    }

    /// Arms or disarms the spin after one spin: a miss disarms it; a catch
    /// keeps it armed until [`FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS`] in a row,
    /// then disarms it so the next empty ring sleeps once (a quick wake
    /// re-arms it). O(1).
    fn note_spin_outcome(&mut self, caught: bool) {
        if caught {
            self.spin_hit_streak = self.spin_hit_streak.saturating_add(1);
            if self.spin_hit_streak < FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS {
                return;
            }
        }
        self.spin_armed = false;
        self.spin_hit_streak = 0;
    }

    /// Adds one spin to the stats. O(1), plain adds, no allocation.
    fn note_spin(&mut self, started: Instant, ended: Instant, hit: bool) {
        let nanos =
            u64::try_from(ended.saturating_duration_since(started).as_nanos()).unwrap_or(u64::MAX);
        let stats = &mut self.spin_stats;
        if hit {
            stats.hits = stats.hits.saturating_add(1);
        } else {
            stats.misses = stats.misses.saturating_add(1);
        }
        stats.spin_nanos = stats.spin_nanos.saturating_add(nanos);
    }

    /// What the short wait before sleeping did since the ring was built.
    /// One copy of three integers.
    #[must_use]
    pub fn spin_stats(&self) -> FrameRingSpinStats {
        self.spin_stats
    }

    /// Whether the consumer has declared it busy-polls. One load.
    #[must_use]
    pub fn is_busy_polling(&self) -> bool {
        self.shared.busy_polling.load(Ordering::Relaxed)
    }

    /// The next item if one is queued now. O(1).
    pub fn try_recv(&mut self) -> Option<T> {
        self.pop_value().ok()
    }

    /// Items queued now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.queue.len()
    }

    /// Whether nothing is queued now.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared.queue.is_empty()
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
    fn test_with_discard_handler_reuses_the_handler_for_an_existing_class() {
        let (tx, rx) = frame_ring::<u8>(4);
        let values = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&values);
        let first = tx.clone().with_discard_handler((1, 0), move |item| {
            recorded.lock().expect("record discarded item").push(*item);
        });
        let same = tx.with_discard_handler((1, 0), |_| {
            panic!("duplicate class installed a new handler")
        });

        first.try_send(4).expect("first item");
        same.try_send(7).expect("second item");
        drop(rx);

        assert_eq!(&*values.lock().expect("read discarded items"), &[4, 7]);
    }

    #[test]
    fn upgraded_sender_keeps_its_discard_class_and_diagnostics_keep_the_key_private() {
        let (plain, rx) = frame_ring::<u8>(2);
        let discarded = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&discarded);
        let class_key: usize = 0x5a5a1234;
        let tagged = plain
            .clone()
            .with_discard_handler((class_key, 1), move |item| {
                recorded.lock().expect("record tagged item").push(*item);
            });
        let weak = tagged.downgrade();
        drop(tagged);

        let upgraded = weak.upgrade().expect("untagged sender still owns the ring");
        let diagnostic = format!("{upgraded:?}");
        assert!(diagnostic.contains("DiscardHandler"));
        assert!(!diagnostic.contains(&class_key.to_string()));
        plain.try_send(7).expect("untagged frame");
        upgraded.try_send(8).expect("tagged frame");
        drop(rx);
        assert_eq!(*discarded.lock().expect("read tagged items"), [8]);
    }

    #[test]
    fn spin_delivers_committed_tail_then_observes_close_with_senders_still_alive() {
        let (tx, mut rx) = frame_ring(2);
        rx.set_spin_before_park(FRAME_RING_SPIN_MAX);
        tx.try_send(91).expect("frame committed before shutdown");
        rx.close();

        // Exercise the same spin check when shutdown wins between recv's first
        // empty probe and its spin. Driving this check directly avoids a timing race.
        assert_eq!(rx.spin_for_next(), Some(Some(91)));
        assert_eq!(rx.spin_for_next(), Some(None));
        assert_eq!(tx.try_send(92), Err(FrameRingTrySendError::Closed(92)));
        let stats = rx.spin_stats();
        assert_eq!((stats.hits, stats.misses), (1, 0));
    }

    #[tokio::test]
    async fn dropping_the_last_sender_while_an_empty_receiver_registers_cannot_strand_it() {
        let (tx, mut rx) = frame_ring::<usize>(1);
        let shared = Arc::clone(&rx.shared);
        let last_sender = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            // Wait until recv has found the ring empty and reached its first
            // waiter registration. Drop may win before or after the recheck.
            while shared.registrations.load(Ordering::Relaxed) == 0 {
                assert!(Instant::now() < deadline, "receiver never began parking");
                std::thread::yield_now();
            }
            drop(tx);
        });

        let outcome = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        last_sender.join().expect("producer completed");
        assert_eq!(outcome.expect("last sender must wake receiver"), None);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn test_close_sets_receiver_closed_and_preserves_already_committed_items() {
        let (tx, mut rx) = frame_ring(2);
        let second = tx.clone();
        tx.try_send(7).expect("queued before close");
        second.try_send(8).expect("ring now full");
        assert!(!tx.receiver_closed());

        rx.close();
        rx.close();

        assert!(second.receiver_closed());
        assert_eq!(tx.try_send(9), Err(FrameRingTrySendError::Closed(9)));
        assert_eq!(rx.recv().await, Some(7));
        assert_eq!(rx.recv().await, Some(8));
        assert_eq!(rx.recv().await, None);
    }

    #[test]
    fn receiver_drop_refunds_only_pending_committed_writes_in_a_close_race() {
        use std::collections::BTreeSet;
        use std::sync::Barrier;
        for capacity in [1, 17, 1024] {
            let (tx, mut rx) = frame_ring::<usize>(capacity);
            let refunded = Arc::new(Mutex::new(BTreeSet::new()));
            let seen = Arc::clone(&refunded);
            let tx = tx.with_discard_handler((1, 0), move |id| {
                assert!(seen.lock().expect("refunds").insert(*id));
            });
            let weak = tx.downgrade();
            tx.try_send(0).expect("first item");
            assert_eq!(rx.try_recv(), Some(0));
            tx.try_send(1).expect("buffered");
            let gate = Barrier::new(5);
            let committed = std::thread::scope(|scope| {
                let mut writers = Vec::new();
                for writer in 0..4 {
                    let tx = tx.clone();
                    let gate = &gate;
                    writers.push(scope.spawn(move || {
                        gate.wait();
                        (0..128)
                            .map(|i| 2 + writer * 128 + i)
                            .filter(|id| tx.try_send(*id).is_ok())
                            .collect::<Vec<_>>()
                    }));
                }
                gate.wait();
                drop(rx);
                let mut committed = BTreeSet::from([1]);
                for writer in writers {
                    committed.extend(writer.join().expect("writer"));
                }
                committed
            });
            assert_eq!(*refunded.lock().expect("refunds"), committed);
            assert_eq!(
                weak.upgrade().expect("strong sender").try_send(999),
                Err(FrameRingTrySendError::Closed(999))
            );
        }
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
    fn test_set_busy_polling_skips_the_wake_only_while_set() {
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
    fn test_is_busy_polling_reads_false_once_recv_parks() {
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

    // ---- US1-B (2026-11-24): short wait before sleeping ----

    /// Off by default: `recv` on an empty ring registers to sleep at once,
    /// exactly as before the spin existed.
    #[test]
    fn test_spin_before_park_defaults_to_zero_and_parks_at_once() {
        let (_tx, mut rx) = frame_ring::<u32>(4);
        assert_eq!(rx.spin_before_park(), Duration::ZERO);
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);
        let shared = Arc::clone(&rx.shared);
        let mut fut = std::pin::pin!(rx.recv());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(shared.registrations.load(Ordering::Relaxed), 1);
        assert!(!shared.busy_polling.load(Ordering::Relaxed));
    }

    /// The setter caps the spin, so a config typo cannot stall the drain.
    #[test]
    fn test_set_spin_before_park_caps_at_the_maximum() {
        let (_tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(Duration::from_micros(50));
        assert_eq!(rx.spin_before_park(), Duration::from_micros(50));
        rx.set_spin_before_park(Duration::from_secs(5));
        assert_eq!(rx.spin_before_park(), FRAME_RING_SPIN_MAX);
        rx.set_spin_before_park(Duration::ZERO);
        assert_eq!(rx.spin_before_park(), Duration::ZERO);
    }

    /// A spin that finds nothing ends with the flag cleared and the drain
    /// registered, and the next push wakes it: no frame is stranded.
    #[test]
    fn test_spin_that_times_out_parks_with_the_flag_cleared_and_is_woken() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(Duration::from_micros(20));
        let shared = Arc::clone(&rx.shared);
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);
        let started = std::time::Instant::now();
        let mut fut = std::pin::pin!(rx.recv());
        assert!(fut.as_mut().poll(&mut cx).is_pending(), "empty: parks");
        assert!(
            started.elapsed() >= Duration::from_micros(20),
            "it spun for its budget before parking"
        );
        assert!(!shared.busy_polling.load(Ordering::SeqCst), "flag cleared");
        assert_eq!(shared.registrations.load(Ordering::Relaxed), 1);
        tx.try_send(7).expect("room");
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "parked drain woken");
        assert_eq!(fut.as_mut().poll(&mut cx), std::task::Poll::Ready(Some(7)));
    }

    /// A frame pushed while the drain spins is taken by the spin: the reader
    /// skips `notify_one` and the drain never registers. Thread timing is not
    /// deterministic, so the scenario is tried until one round lands inside
    /// the spin (each round still checks the frame is delivered).
    #[test]
    fn test_frame_pushed_during_the_spin_is_taken_with_no_wake() {
        let mut caught_in_spin = false;
        for _ in 0..50 {
            let (tx, mut rx) = frame_ring::<u32>(4);
            rx.set_spin_before_park(FRAME_RING_SPIN_MAX);
            let shared = Arc::clone(&rx.shared);
            let watcher = Arc::clone(&rx.shared);
            let producer = std::thread::spawn(move || {
                // Bounded: a round whose spin ended before this thread ran
                // still sends, through the ordinary wake (review: an
                // unbounded wait hung the test).
                let deadline = std::time::Instant::now() + Duration::from_millis(20);
                while !watcher.busy_polling.load(Ordering::SeqCst)
                    && std::time::Instant::now() < deadline
                {
                    std::hint::spin_loop();
                }
                tx.try_send(42).expect("room");
            });
            let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
            let waker = std::task::Waker::from(Arc::clone(&counter));
            let mut cx = std::task::Context::from_waker(&waker);
            let mut fut = std::pin::pin!(rx.recv());
            let got = loop {
                if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                    break v;
                }
                std::thread::yield_now();
            };
            producer.join().expect("producer");
            assert_eq!(got, Some(42), "every round delivers the frame");
            if shared.notifies.load(Ordering::Relaxed) == 0
                && shared.registrations.load(Ordering::Relaxed) == 0
            {
                caught_in_spin = true;
                break;
            }
        }
        assert!(caught_in_spin, "no round in 50 landed inside a 1 ms spin");
    }

    /// The spin ends at once when the last sender is gone.
    #[tokio::test]
    async fn test_spin_returns_none_once_every_sender_is_gone() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(FRAME_RING_SPIN_MAX);
        tx.try_send(1).expect("room");
        drop(tx);
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, None);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// With a random spin, several producers pushing with random pauses
        /// deliver every item exactly once and the drain always ends: a frame
        /// stranded by a skipped wake would hang into the timeout.
        #[test]
        fn proptest_spin_before_park_never_strands_or_repeats_a_frame(
            spin_us in 0_u64..300,
            producers in 1_u64..5,
            per_producer in 1_u64..3_000,
            pause_every in 1_u64..64,
            pause_spins in 0_u32..40_000,
            capacity in 1_usize..256,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime");
            let (tx, mut rx) = frame_ring::<u64>(capacity);
            rx.set_spin_before_park(Duration::from_micros(spin_us));
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
            let result = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(30), async {
                    let mut seen = vec![0_u32; (producers * per_producer) as usize];
                    while let Some(v) = rx.recv().await {
                        seen[v as usize] += 1;
                    }
                    seen
                })
                .await
            });
            for h in handles {
                h.join().expect("producer");
            }
            let seen = result.expect("a stranded frame hangs the drain");
            proptest::prop_assert!(seen.iter().all(|&n| n == 1), "each item exactly once");
        }
    }

    /// The stats count a miss with its time, then a hit; an unspun ring
    /// counts nothing.
    #[test]
    fn test_spin_stats_count_hits_misses_and_time() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        assert_eq!(rx.spin_stats(), FrameRingSpinStats::default());
        rx.set_spin_before_park(Duration::from_micros(20));
        assert_eq!(rx.spin_for_next(), None, "empty: the spin runs out");
        let after_miss = rx.spin_stats();
        assert_eq!((after_miss.hits, after_miss.misses), (0, 1));
        assert!(after_miss.spin_nanos >= 20_000, "the miss spun its budget");
        tx.try_send(3).expect("room");
        assert_eq!(rx.spin_for_next(), Some(Some(3)));
        let after_hit = rx.spin_stats();
        assert_eq!((after_hit.hits, after_hit.misses), (1, 1));
        assert!(after_hit.spin_nanos >= after_miss.spin_nanos);
    }

    /// Adaptive spin: a spin that runs out disarms it, so a slow feed pays
    /// one spin and then sleeps at once; a sleep longer than the spin keeps
    /// it off; a sleep shorter than the spin (the next frame came soon)
    /// re-arms it.
    #[test]
    fn test_spin_disarms_after_a_miss_and_rearms_after_a_short_sleep() {
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);

        // A miss disarms; a long sleep keeps it off.
        let (tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(Duration::from_micros(20));
        assert!(rx.spin_armed, "armed from the start");
        {
            let mut fut = std::pin::pin!(rx.recv());
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            std::thread::sleep(Duration::from_millis(5));
            tx.try_send(1).expect("room");
            assert_eq!(fut.as_mut().poll(&mut cx), std::task::Poll::Ready(Some(1)));
        }
        assert!(!rx.spin_armed, "a 5 ms sleep is longer than a 20 µs spin");
        assert_eq!(rx.spin_stats().misses, 1);
        {
            let mut fut = std::pin::pin!(rx.recv());
            assert!(fut.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(rx.spin_stats().misses, 1, "disarmed: no second spin");

        // A sleep shorter than the spin re-arms (timing: retried).
        let mut rearmed = false;
        for _ in 0..20 {
            let (tx, mut rx) = frame_ring::<u32>(4);
            rx.set_spin_before_park(FRAME_RING_SPIN_MAX);
            rx.spin_armed = false;
            {
                let mut fut = std::pin::pin!(rx.recv());
                assert!(fut.as_mut().poll(&mut cx).is_pending(), "disarmed: parks");
                tx.try_send(2).expect("room");
                assert_eq!(fut.as_mut().poll(&mut cx), std::task::Poll::Ready(Some(2)));
            }
            if rx.spin_armed {
                rearmed = true;
                break;
            }
        }
        assert!(rearmed, "a wake well under 1 ms must re-arm the spin");
    }

    /// Regression (US1-B review, 2026-11-24): a sleep right after a missed
    /// spin starts one budget late, so a frame that ends it inside the
    /// budget came one to two budgets after the gap began, where a spin can
    /// never catch it. It must NOT re-arm the spin (before the fix it did,
    /// so gaps of 50 to 100 µs paid a full spin AND the wake every time).
    #[test]
    fn test_regression_a_quick_wake_after_a_missed_spin_does_not_rearm() {
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);
        let (tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(FRAME_RING_SPIN_MAX);
        assert!(rx.spin_armed);
        {
            let mut fut = std::pin::pin!(rx.recv());
            assert!(
                fut.as_mut().poll(&mut cx).is_pending(),
                "spun 1 ms, missed, parked"
            );
            tx.try_send(5).expect("room");
            assert_eq!(fut.as_mut().poll(&mut cx), std::task::Poll::Ready(Some(5)));
        }
        assert_eq!(rx.spin_stats().misses, 1);
        assert!(!rx.spin_armed, "the sleep after a miss never re-arms");
    }

    /// Regression (US1-B review, 2026-11-24): a stale `Notify` permit (a
    /// push while the drain ran unregistered with the flag clear) ends the
    /// next sleep at once with nothing queued. That empty wake must not
    /// re-arm the spin.
    #[test]
    fn test_regression_a_stale_permit_wake_does_not_rearm() {
        let counter = Arc::new(CountingWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = std::task::Context::from_waker(&waker);
        let (tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(FRAME_RING_SPIN_MAX);
        rx.spin_armed = false;
        // Flag clear, nobody registered: this push stores a permit.
        tx.try_send(1).expect("room");
        assert_eq!(rx.try_recv(), Some(1), "taken without registering");
        {
            let mut fut = std::pin::pin!(rx.recv());
            assert!(
                fut.as_mut().poll(&mut cx).is_pending(),
                "the stale permit wakes nothing up: parked again"
            );
        }
        assert!(!rx.spin_armed, "an empty wake re-arms nothing");
        assert_eq!(rx.spin_stats().misses, 0, "disarmed: no spin");
    }

    /// After `FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS` catches in a row the spin
    /// disarms, so the next empty ring sleeps once (the caller's other
    /// `select!` arms are polled) instead of spinning through a whole rush.
    #[test]
    fn test_spin_disarms_after_the_consecutive_hit_cap() {
        let (tx, mut rx) = frame_ring::<u32>(4);
        rx.set_spin_before_park(Duration::from_micros(20));
        for i in 0..FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS {
            assert!(rx.spin_armed, "armed before catch {i}");
            tx.try_send(i).expect("room");
            // The fast path would take a queued frame; go through the spin,
            // then the same outcome rule `recv` applies.
            let caught = rx.spin_for_next();
            rx.note_spin_outcome(caught.is_some());
            assert_eq!(caught, Some(Some(i)));
        }
        assert!(!rx.spin_armed, "the cap disarms the spin");
        assert_eq!(
            rx.spin_stats().hits,
            u64::from(FRAME_RING_SPIN_MAX_CONSECUTIVE_HITS)
        );
    }
}
