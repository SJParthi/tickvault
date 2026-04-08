//! Order idempotency using UUID v4 correlation IDs.
//!
//! Each order placed through the OMS gets a unique correlation ID.
//! Dhan echoes this ID back in the place response and in WebSocket order updates,
//! enabling matching between our request and the resulting order.
//!
//! Dhan `correlationId` max length is **30 characters**, charset `[a-zA-Z0-9 _-]`.
//! UUID v4 (36 chars) exceeds this limit, so we strip hyphens to produce a
//! 32-char hex string, then truncate to 30 chars. This preserves 120 bits of
//! entropy (30 hex chars = 120 bits), which is collision-safe for our volume.
//!
//! # Phase 1 (current)
//! In-memory `HashMap<String, String>` mapping `correlation_id → order_id`.
//! Sufficient for single-instance deployment.
//!
//! **Known limitation:** On crash, all mappings are lost. The reconciliation
//! module (`reconciliation.rs`) re-syncs state from Dhan REST API on startup.
//! Phase 2 will persist the map to Valkey for fast recovery.

use std::collections::HashMap;

use dhan_live_trader_common::constants::DHAN_CORRELATION_ID_MAX_LENGTH;
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
            index: HashMap::with_capacity(256),
        }
    }

    /// Generates a new correlation ID from UUID v4, truncated to 30 chars.
    ///
    /// Dhan `correlationId` max length is 30 characters with charset `[a-zA-Z0-9 _-]`.
    /// UUID v4 simple (no hyphens) = 32 hex chars; we take first 30 for Dhan compliance.
    ///
    /// # Returns
    /// A 30-character hex string (120 bits of entropy).
    pub fn generate_id(&self) -> String {
        let uuid = Uuid::new_v4();
        let hex = uuid.as_simple().to_string(); // 32 hex chars, no hyphens
        hex[..DHAN_CORRELATION_ID_MAX_LENGTH].to_string()
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
    fn generate_id_fits_dhan_correlation_id_limit() {
        let tracker = CorrelationTracker::new();
        let id = tracker.generate_id();
        assert_eq!(id.len(), DHAN_CORRELATION_ID_MAX_LENGTH);
        // Must be valid hex (subset of Dhan's allowed charset [a-zA-Z0-9 _-])
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_id_returns_unique_ids() {
        let tracker = CorrelationTracker::new();
        let id1 = tracker.generate_id();
        let id2 = tracker.generate_id();
        assert_ne!(id1, id2);
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
    fn default_trait_creates_empty_tracker() {
        let tracker = CorrelationTracker::default();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
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
