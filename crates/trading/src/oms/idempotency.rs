//! Order idempotency using UUID v4 correlation IDs.
//!
//! Each order placed through the OMS gets a unique correlation ID (UUID v4).
//! Dhan echoes this ID back in the place response and in WebSocket order updates,
//! enabling matching between our request and the resulting order.
//!
//! # Phase 1
//! In-memory `HashMap<String, String>` mapping `correlation_id → order_id`.
//! Sufficient for single-instance deployment.

use std::collections::HashMap;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// CorrelationTracker
// ---------------------------------------------------------------------------

/// Tracks correlation ID to order ID mappings for idempotency.
///
/// Used to match Dhan's asynchronous order responses with our requests.
pub struct CorrelationTracker {
    /// Maps correlation_id (our UUID) → order_id (Dhan's ID).
    index: HashMap<String, String>,
}

impl Default for CorrelationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CorrelationTracker {
    /// Creates a new empty tracker.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
        }
    }

    /// Generates a new correlation ID (≤30 chars, Dhan API limit).
    ///
    /// Uses UUID v4 simple (hex, no hyphens) truncated to 30 characters.
    /// Charset: `[0-9a-f]` — valid within Dhan's `[a-zA-Z0-9 _-]` allowlist.
    ///
    /// # Returns
    /// A 30-character unique correlation ID string.
    pub fn generate_id(&self) -> String {
        // UUID v4 simple = 32 hex chars. Dhan max = 30. Truncate last 2.
        // 30 hex chars = 120 bits of entropy — collision risk negligible.
        let uuid = Uuid::new_v4().simple().to_string();
        // CORRELATION_ID_MAX_LENGTH: Dhan enforces max 30 chars on correlationId.
        const CORRELATION_ID_MAX_LENGTH: usize = 30;
        uuid[..CORRELATION_ID_MAX_LENGTH].to_owned()
    }

    /// Records a mapping from correlation ID to order ID.
    ///
    /// Called after a successful place order response from Dhan.
    pub fn track(&mut self, correlation_id: String, order_id: String) {
        self.index.insert(correlation_id, order_id);
    }

    /// Looks up the order ID for a given correlation ID.
    pub fn get_order_id(&self, correlation_id: &str) -> Option<&String> {
        self.index.get(correlation_id)
    }

    /// Returns true if the correlation ID is tracked.
    pub fn contains(&self, correlation_id: &str) -> bool {
        self.index.contains_key(correlation_id)
    }

    /// Returns the number of tracked correlations.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Returns true if no correlations are tracked.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Clears all tracked correlations (daily reset).
    pub fn clear(&mut self) {
        self.index.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_id_returns_30_char_hex_string() {
        let tracker = CorrelationTracker::new();
        let id = tracker.generate_id();
        assert_eq!(
            id.len(),
            30,
            "correlationId must be exactly 30 chars (Dhan limit)"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "correlationId must be hex chars only"
        );
    }

    #[test]
    fn generate_id_returns_unique_ids() {
        let tracker = CorrelationTracker::new();
        let id1 = tracker.generate_id();
        let id2 = tracker.generate_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_correlation_id_max_length_30() {
        let tracker = CorrelationTracker::new();
        for _ in 0..100 {
            let id = tracker.generate_id();
            assert!(
                id.len() <= 30,
                "correlationId exceeds Dhan 30-char max: len={}, id={}",
                id.len(),
                id
            );
        }
    }

    #[test]
    fn track_and_lookup() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("corr-1".to_owned(), "order-100".to_owned());

        assert_eq!(
            tracker.get_order_id("corr-1"),
            Some(&"order-100".to_owned())
        );
        assert!(tracker.contains("corr-1"));
        assert!(!tracker.contains("corr-2"));
    }

    #[test]
    fn new_tracker_is_empty() {
        let tracker = CorrelationTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn clear_removes_all() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("c1".to_owned(), "o1".to_owned());
        tracker.track("c2".to_owned(), "o2".to_owned());
        assert_eq!(tracker.len(), 2);

        tracker.clear();
        assert!(tracker.is_empty());
        assert!(tracker.get_order_id("c1").is_none());
    }

    #[test]
    fn multiple_correlations() {
        let mut tracker = CorrelationTracker::new();
        tracker.track("c1".to_owned(), "o1".to_owned());
        tracker.track("c2".to_owned(), "o2".to_owned());
        tracker.track("c3".to_owned(), "o3".to_owned());

        assert_eq!(tracker.len(), 3);
        assert_eq!(tracker.get_order_id("c1"), Some(&"o1".to_owned()));
        assert_eq!(tracker.get_order_id("c2"), Some(&"o2".to_owned()));
        assert_eq!(tracker.get_order_id("c3"), Some(&"o3".to_owned()));
    }
}
