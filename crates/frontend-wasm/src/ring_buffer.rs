//! Fixed-size ring buffer for indicator lookback windows.
//!
//! Pre-allocated at indicator creation. Zero allocation on the hot path.
//! Provides O(1) push, O(1) indexed access, and O(1) aggregate operations
//! (sum, mean) via running accumulators.

// ---------------------------------------------------------------------------
// Ring Buffer
// ---------------------------------------------------------------------------

/// Fixed-capacity ring buffer of f64 values for indicator lookback windows.
///
/// # Performance
/// - `push()`: O(1) — single index write + wrap
/// - `get()`: O(1) — single index read + wrap
/// - `sum()` / `mean()`: O(1) — maintained via running accumulator
///
/// # Capacity
/// Set at creation time. Cannot grow. This is intentional — prevents
/// accidental heap allocation on the hot path.
pub struct RingBuffer {
    /// Pre-allocated storage.
    data: Box<[f64]>,
    /// Current write position (wraps at capacity).
    head: usize,
    /// Number of values currently stored (0..=capacity).
    len: usize,
    /// Total capacity (fixed at creation).
    capacity: usize,
    /// Running sum for O(1) mean calculation.
    running_sum: f64,
}

impl RingBuffer {
    /// Creates a new ring buffer with the given capacity.
    ///
    /// All slots initialized to 0.0. Running sum starts at 0.0.
    pub fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0, "ring buffer capacity must be > 0");
        Self {
            data: vec![0.0; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
            capacity,
            running_sum: 0.0,
        }
    }

    /// Pushes a value, evicting the oldest if full.
    ///
    /// # Performance
    /// O(1) — one subtract, one add, one index write.
    #[inline(always)]
    pub fn push(&mut self, value: f64) {
        if self.len == self.capacity {
            // Subtract the value being evicted from running sum
            self.running_sum -= self.data[self.head];
        } else {
            self.len += 1;
        }
        self.data[self.head] = value;
        self.running_sum += value;
        self.head = (self.head + 1) % self.capacity;
    }

    /// Returns the value at the given index (0 = most recent).
    ///
    /// # Performance
    /// O(1) — single array access with wrap.
    #[inline(always)]
    pub fn get(&self, index: usize) -> f64 {
        debug_assert!(index < self.len, "ring buffer index out of bounds");
        // head points to next write position, so most recent is at head - 1
        let actual = (self.head + self.capacity - 1 - index) % self.capacity;
        self.data[actual]
    }

    /// Returns the running sum of all stored values.
    ///
    /// # Performance
    /// O(1) — returns pre-computed accumulator.
    #[inline(always)]
    pub fn sum(&self) -> f64 {
        self.running_sum
    }

    /// Returns the mean of all stored values.
    ///
    /// Returns NaN if empty.
    ///
    /// # Performance
    /// O(1) — one division.
    #[inline(always)]
    pub fn mean(&self) -> f64 {
        if self.len == 0 {
            return f64::NAN;
        }
        self.running_sum / self.len as f64
    }

    /// Number of values currently stored.
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the buffer is fully populated.
    #[inline(always)]
    pub fn is_full(&self) -> bool {
        self.len == self.capacity
    }

    /// Maximum capacity.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Resets the buffer to empty state. No deallocation.
    pub fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
        self.running_sum = 0.0;
        // Zero out data to prevent stale reads after reset
        for slot in self.data.iter_mut() {
            *slot = 0.0;
        }
    }

    /// Returns the highest value in the buffer.
    ///
    /// Returns NaN if empty.
    ///
    /// # Performance
    /// O(n) where n = len. Use sparingly — not for hot-path per-tick calls.
    /// For indicators needing O(1) max, maintain a separate max-tracking structure.
    pub fn max(&self) -> f64 {
        if self.len == 0 {
            return f64::NAN;
        }
        let mut max_val = f64::NEG_INFINITY;
        for i in 0..self.len {
            let val = self.get(i);
            if val > max_val {
                max_val = val;
            }
        }
        max_val
    }

    /// Returns the lowest value in the buffer.
    ///
    /// Returns NaN if empty.
    ///
    /// # Performance
    /// O(n) where n = len. Same caveat as `max()`.
    pub fn min(&self) -> f64 {
        if self.len == 0 {
            return f64::NAN;
        }
        let mut min_val = f64::INFINITY;
        for i in 0..self.len {
            let val = self.get(i);
            if val < min_val {
                min_val = val;
            }
        }
        min_val
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_buffer_is_empty() {
        let buf = RingBuffer::new(10);
        assert!(buf.is_empty());
        assert!(!buf.is_full());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.capacity(), 10);
    }

    #[test]
    fn test_push_and_get_single() {
        let mut buf = RingBuffer::new(5);
        buf.push(42.0);
        assert_eq!(buf.len(), 1);
        assert!((buf.get(0) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_push_and_get_multiple() {
        let mut buf = RingBuffer::new(5);
        buf.push(1.0);
        buf.push(2.0);
        buf.push(3.0);
        assert_eq!(buf.len(), 3);
        assert!((buf.get(0) - 3.0).abs() < f64::EPSILON); // most recent
        assert!((buf.get(1) - 2.0).abs() < f64::EPSILON);
        assert!((buf.get(2) - 1.0).abs() < f64::EPSILON); // oldest
    }

    #[test]
    fn test_wrap_around_eviction() {
        let mut buf = RingBuffer::new(3);
        buf.push(1.0);
        buf.push(2.0);
        buf.push(3.0);
        assert!(buf.is_full());
        buf.push(4.0); // evicts 1.0
        assert_eq!(buf.len(), 3);
        assert!((buf.get(0) - 4.0).abs() < f64::EPSILON);
        assert!((buf.get(1) - 3.0).abs() < f64::EPSILON);
        assert!((buf.get(2) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_running_sum() {
        let mut buf = RingBuffer::new(3);
        buf.push(10.0);
        assert!((buf.sum() - 10.0).abs() < f64::EPSILON);
        buf.push(20.0);
        assert!((buf.sum() - 30.0).abs() < f64::EPSILON);
        buf.push(30.0);
        assert!((buf.sum() - 60.0).abs() < f64::EPSILON);
        buf.push(40.0); // evicts 10.0
        assert!((buf.sum() - 90.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_mean() {
        let mut buf = RingBuffer::new(4);
        buf.push(10.0);
        buf.push(20.0);
        buf.push(30.0);
        buf.push(40.0);
        assert!((buf.mean() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_mean_empty_is_nan() {
        let buf = RingBuffer::new(5);
        assert!(buf.mean().is_nan());
    }

    #[test]
    fn test_max_and_min() {
        let mut buf = RingBuffer::new(5);
        buf.push(3.0);
        buf.push(1.0);
        buf.push(5.0);
        buf.push(2.0);
        assert!((buf.max() - 5.0).abs() < f64::EPSILON);
        assert!((buf.min() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_max_min_empty_is_nan() {
        let buf = RingBuffer::new(5);
        assert!(buf.max().is_nan());
        assert!(buf.min().is_nan());
    }

    #[test]
    fn test_reset() {
        let mut buf = RingBuffer::new(5);
        buf.push(1.0);
        buf.push(2.0);
        buf.push(3.0);
        buf.reset();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert!((buf.sum() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_push_after_reset() {
        let mut buf = RingBuffer::new(3);
        buf.push(100.0);
        buf.push(200.0);
        buf.reset();
        buf.push(50.0);
        assert_eq!(buf.len(), 1);
        assert!((buf.get(0) - 50.0).abs() < f64::EPSILON);
        assert!((buf.sum() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_capacity_one() {
        let mut buf = RingBuffer::new(1);
        buf.push(1.0);
        assert!(buf.is_full());
        assert!((buf.get(0) - 1.0).abs() < f64::EPSILON);
        buf.push(2.0);
        assert!((buf.get(0) - 2.0).abs() < f64::EPSILON);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn test_large_capacity() {
        let mut buf = RingBuffer::new(10_000);
        for i in 0..10_000 {
            buf.push(i as f64);
        }
        assert!(buf.is_full());
        assert!((buf.get(0) - 9999.0).abs() < f64::EPSILON);
        assert!((buf.get(9999) - 0.0).abs() < f64::EPSILON);
    }
}
