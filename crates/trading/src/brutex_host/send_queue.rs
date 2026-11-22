//! A fixed-capacity priority queue for order intents waiting on rate limits.
//!
//! Ordered by `(rank ascending, arrival sequence ascending)`: the lowest rank
//! leaves first, and intents of equal rank leave in the order they arrived.
//!
//! Complexity, stated honestly: `push` and `pop` are **O(log N)**, not O(1)
//! (a binary heap's sift). `N` is a compile-time capacity held to at most 64,
//! so a sift is at most 6 levels. No allocation ever: the heap is a fixed
//! array inside the struct. A push into a full queue is refused and counted,
//! never grown.

/// The largest capacity the queue may be built with.
pub const SEND_QUEUE_MAX_CAPACITY: usize = 64;

/// The queue is full; the item was not added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("send queue is full")]
pub struct QueueFull;

#[derive(Debug, Clone, Copy)]
struct Entry<T: Copy> {
    item: T,
    rank: u32,
    seq: u64,
    enqueue_ns: i64,
}

impl<T: Copy> Entry<T> {
    #[inline]
    const fn key(&self) -> (u32, u64) {
        (self.rank, self.seq)
    }
}

/// See the module docs.
#[derive(Debug, Clone, Copy)]
pub struct SendQueue<T: Copy, const N: usize> {
    heap: [Option<Entry<T>>; N],
    len: usize,
    next_seq: u64,
    peak_len: usize,
    refused_full: u64,
}

impl<T: Copy, const N: usize> Default for SendQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy, const N: usize> SendQueue<T, N> {
    /// An empty queue. `N` must be 1..=64 (checked at compile time).
    #[must_use]
    pub const fn new() -> Self {
        const {
            assert!(
                N > 0 && N <= SEND_QUEUE_MAX_CAPACITY,
                "SendQueue capacity must be 1..=64"
            );
        }
        Self {
            heap: [None; N],
            len: 0,
            next_seq: 0,
            peak_len: 0,
            refused_full: 0,
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// The most items ever held at once.
    #[must_use]
    pub const fn peak_len(&self) -> usize {
        self.peak_len
    }

    /// Pushes refused because the queue was full.
    #[must_use]
    pub const fn refused_full(&self) -> u64 {
        self.refused_full
    }

    #[inline]
    fn key_at(&self, i: usize) -> (u32, u64) {
        match self.heap.get(i) {
            Some(Some(e)) => e.key(),
            // Unreachable for i < len; sorts last so it never moves up.
            _ => (u32::MAX, u64::MAX),
        }
    }

    /// Adds an item. O(log N), no allocation. The arrival sequence is a
    /// `u64` counter (wraps only after 2^64 pushes).
    pub fn push(&mut self, item: T, rank: u32, enqueue_ns: i64) -> Result<(), QueueFull> {
        if self.len >= N {
            self.refused_full = self.refused_full.saturating_add(1);
            return Err(QueueFull);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let mut i = self.len;
        if let Some(slot) = self.heap.get_mut(i) {
            *slot = Some(Entry {
                item,
                rank,
                seq,
                enqueue_ns,
            });
        }
        self.len += 1;
        if self.len > self.peak_len {
            self.peak_len = self.len;
        }
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.key_at(i) < self.key_at(parent) {
                self.heap.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Removes the first item and how long it waited (`now_ns − enqueue_ns`,
    /// 0 if the clock went back). O(log N), no allocation.
    pub fn pop(&mut self, now_ns: i64) -> Option<(T, u64)> {
        if self.len == 0 {
            return None;
        }
        let last = self.len - 1;
        self.heap.swap(0, last);
        let top = self.heap.get_mut(last).and_then(Option::take)?;
        self.len = last;
        let mut i = 0;
        loop {
            let l = 2 * i + 1;
            if l >= self.len {
                break;
            }
            let r = l + 1;
            let child = if r < self.len && self.key_at(r) < self.key_at(l) {
                r
            } else {
                l
            };
            if self.key_at(child) < self.key_at(i) {
                self.heap.swap(i, child);
                i = child;
            } else {
                break;
            }
        }
        let wait = now_ns.saturating_sub(top.enqueue_ns).max(0);
        Some((top.item, u64::try_from(wait).unwrap_or(0)))
    }

    /// Empties the queue (counters kept). O(N).
    pub fn clear(&mut self) {
        for slot in &mut self.heap {
            *slot = None;
        }
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_by_rank_then_arrival() {
        let mut q: SendQueue<u32, 8> = SendQueue::new();
        assert!(q.is_empty());
        q.push(1, 5, 0).unwrap();
        q.push(2, 1, 10).unwrap();
        q.push(3, 5, 20).unwrap();
        q.push(4, 1, 30).unwrap();
        q.push(5, 0, 40).unwrap();
        assert_eq!(q.len(), 5);
        let order: Vec<u32> = std::iter::from_fn(|| q.pop(100).map(|(v, _)| v)).collect();
        assert_eq!(order, vec![5, 2, 4, 1, 3]);
        assert!(q.is_empty());
        assert_eq!(q.peak_len(), 5);
    }

    #[test]
    fn full_queue_refuses_and_counts() {
        let mut q: SendQueue<u8, 2> = SendQueue::new();
        assert_eq!(q.capacity(), 2);
        q.push(1, 0, 0).unwrap();
        q.push(2, 0, 0).unwrap();
        assert_eq!(q.push(3, 0, 0), Err(QueueFull));
        assert_eq!(q.refused_full(), 1);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn wait_is_measured_and_never_negative() {
        let mut q: SendQueue<u8, 4> = SendQueue::default();
        q.push(7, 0, 1_000).unwrap();
        assert_eq!(q.pop(1_750), Some((7, 750)));
        q.push(8, 0, 1_000).unwrap();
        assert_eq!(q.pop(500), Some((8, 0)));
        assert_eq!(q.pop(500), None);
    }

    #[test]
    fn clear_keeps_counters() {
        let mut q: SendQueue<u8, 1> = SendQueue::new();
        q.push(1, 0, 0).unwrap();
        assert_eq!(q.push(2, 0, 0), Err(QueueFull));
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.refused_full(), 1);
        assert_eq!(q.peak_len(), 1);
        q.push(3, 0, 0).unwrap();
        assert_eq!(q.pop(0), Some((3, 0)));
    }
}
