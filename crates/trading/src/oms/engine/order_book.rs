//! The OMS order map with an O(1) count of active (non-terminal) orders.
//!
//! Audit plan PR6 (`active-plan-audit-fixes-2026-09-26.md`): the order runtime
//! asks `active_order_count()` once a second and after every order event and
//! mark batch, and that answer was a scan of every order held, terminal ones
//! included: O(orders), bounded by `MAX_TRACKED_ORDERS` (25,000).
//!
//! `OrderBook` keeps the set of active order keys beside the map. Every way
//! to change an order goes through this type: `insert` and `clear` adjust the
//! set directly, and `get_mut` hands out an [`OrderMut`] guard that remembers
//! whether the order was active and corrects the set when the guard is
//! dropped. So no call site has to remember to update anything, and a new
//! mutation site cannot bypass the set without changing this file (the map is
//! private here).
//!
//! 2026-11-23 (PR6, index half): the count became a SET of keys, so the
//! active orders can be walked without visiting the terminal ones
//! ([`OrderBook::active_values`]). `active_orders()` and the order runtime's
//! paper-order rebuild walk the active set instead of every order of the day.
//!
//! # Complexity
//! `active_count` is O(1). `active_values` is O(set capacity) plus one map
//! probe per active order: a hash-set walk visits every bucket, and the set
//! keeps its capacity across the daily `clear` (the 256-order prefault, or
//! the peak of orders open at once since boot, whichever is larger; never
//! shrunk, so the placement path does not reallocate). `insert`,
//! `get_mut` and the guard's drop are O(1) average (hash probes plus two
//! `is_terminal` matches). No allocation beyond the map's own: the map key
//! is an `Arc<str>` built once by the caller, and the active set shares it
//! (a reference-count bump), so a placement still allocates exactly what it
//! did before this change (pinned by the order-path DHAT gate,
//! `crates/app/tests/dhat_order_path_recorder.rs`). The one exception is a
//! terminal order made active again through `get_mut`, which the state
//! machine never allows: it allocates a fresh key. `clear` is O(n) in the
//! map, as before.
//!
//! # Honest limits
//! - A guard leaked with `std::mem::forget` would skip its correction. Nothing
//!   in the engine does that; `OrderManagementSystem::active_order_count`
//!   cross-checks the count against the full scan in every debug build, so the
//!   whole test suite exercises it.
//! - The set is per map ENTRY (map key), exactly as the old scan counted.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::oms::types::ManagedOrder;

/// The order map, keyed by order id, hashed with `ahash`. The key is an
/// `Arc<str>` so the active set below shares it (a reference-count bump, no
/// allocation) instead of holding a second copy of every open order's id.
pub(crate) type OrderMap = HashMap<Arc<str>, ManagedOrder, ahash::RandomState>;

/// The OMS order map plus the keys of its non-terminal orders.
#[derive(Debug)]
pub(crate) struct OrderBook {
    map: OrderMap,
    active: HashSet<Arc<str>, ahash::RandomState>,
}

impl OrderBook {
    /// An empty book pre-sized for `capacity` orders.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity_and_hasher(capacity, ahash::RandomState::new()),
            active: HashSet::with_capacity_and_hasher(capacity, ahash::RandomState::new()),
        }
    }

    /// The underlying map, read-only.
    pub(crate) fn map(&self) -> &OrderMap {
        &self.map
    }

    /// Number of non-terminal orders. O(1).
    pub(crate) fn active_count(&self) -> usize {
        self.active.len()
    }

    /// The non-terminal orders, in no particular order. O(set capacity) plus
    /// one map probe per active order (see the module complexity note):
    /// terminal orders (which pile up until the daily reset) are not visited.
    pub(crate) fn active_values(&self) -> impl Iterator<Item = &ManagedOrder> {
        self.active.iter().filter_map(|key| self.map.get(key))
    }

    /// Number of map entries (active and terminal).
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the book holds no order.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether an order is held under `key`.
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// The order held under `key`.
    pub(crate) fn get(&self, key: &str) -> Option<&ManagedOrder> {
        self.map.get(key)
    }

    /// All orders, active and terminal.
    pub(crate) fn values(&self) -> std::collections::hash_map::Values<'_, Arc<str>, ManagedOrder> {
        self.map.values()
    }

    /// Insert or replace the order under `key`, keeping the active set exact.
    ///
    /// Pass the id as `&str` (one allocation, for the shared key) rather than
    /// a cloned `String`, which would allocate twice. The active set takes a
    /// clone of the `Arc`: a reference-count bump, no allocation.
    pub(crate) fn insert(
        &mut self,
        key: impl Into<Arc<str>>,
        order: ManagedOrder,
    ) -> Option<ManagedOrder> {
        let key: Arc<str> = key.into();
        if order.is_terminal() {
            self.active.remove(&key);
        } else {
            self.active.insert(Arc::clone(&key));
        }
        self.map.insert(key, order)
    }

    /// Remove every order. O(n) in the map.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.active.clear();
    }

    /// A mutable handle to the order under `key`. The active set is corrected
    /// when the handle is dropped, whatever the caller changed.
    pub(crate) fn get_mut<'a, 'k>(&'a mut self, key: &'k str) -> Option<OrderMut<'a, 'k>> {
        let Self { map, active } = self;
        map.get_mut(key).map(|order| OrderMut {
            was_active: !order.is_terminal(),
            order,
            active,
            key,
        })
    }
}

/// A mutable order handle that keeps the active set exact.
#[derive(Debug)]
pub(crate) struct OrderMut<'a, 'k> {
    order: &'a mut ManagedOrder,
    active: &'a mut HashSet<Arc<str>, ahash::RandomState>,
    key: &'k str,
    was_active: bool,
}

impl Deref for OrderMut<'_, '_> {
    type Target = ManagedOrder;
    fn deref(&self) -> &ManagedOrder {
        self.order
    }
}

impl DerefMut for OrderMut<'_, '_> {
    fn deref_mut(&mut self) -> &mut ManagedOrder {
        self.order
    }
}

impl Drop for OrderMut<'_, '_> {
    fn drop(&mut self) {
        let is_active = !self.order.is_terminal();
        match (self.was_active, is_active) {
            (true, false) => {
                self.active.remove(self.key);
            }
            (false, true) => {
                // A terminal order becoming active again: not a transition
                // the state machine allows, so this allocation is never paid
                // on a normal order path. Kept so the set stays exact anyway.
                self.active.insert(Arc::from(self.key));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tickvault_common::order_types::{
        OrderStatus, OrderType, OrderValidity, ProductType, TransactionType,
    };
    use tickvault_common::types::ExchangeSegment;

    const ALL: [OrderStatus; 10] = [
        OrderStatus::Transit,
        OrderStatus::Pending,
        OrderStatus::Confirmed,
        OrderStatus::PartTraded,
        OrderStatus::Traded,
        OrderStatus::Cancelled,
        OrderStatus::Rejected,
        OrderStatus::Expired,
        OrderStatus::Closed,
        OrderStatus::Triggered,
    ];

    fn order(id: &str, status: OrderStatus) -> ManagedOrder {
        ManagedOrder {
            order_id: id.to_owned(),
            correlation_id: format!("c-{id}"),
            security_id: 100,
            exchange_segment: ExchangeSegment::NseFno,
            transaction_type: TransactionType::Buy,
            order_type: OrderType::Limit,
            product_type: ProductType::Intraday,
            validity: OrderValidity::Day,
            quantity: 50,
            price: 100.0,
            trigger_price: 0.0,
            status,
            traded_qty: 0,
            avg_traded_price: 0.0,
            lot_size: 25,
            created_at_us: 0,
            updated_at_us: 0,
            needs_reconciliation: false,
            modification_count: 0,
        }
    }

    fn scan(book: &OrderBook) -> usize {
        book.values().filter(|o| !o.is_terminal()).count()
    }

    /// The active orders' ids, sorted, from the index and from a full scan.
    fn ids_both_ways(book: &OrderBook) -> (Vec<String>, Vec<String>) {
        let mut indexed: Vec<String> = book.active_values().map(|o| o.order_id.clone()).collect();
        let mut scanned: Vec<String> = book
            .values()
            .filter(|o| !o.is_terminal())
            .map(|o| o.order_id.clone())
            .collect();
        indexed.sort();
        scanned.sort();
        (indexed, scanned)
    }

    #[test]
    fn test_active_values_walks_only_open_orders() {
        let mut book = OrderBook::with_capacity(4);
        book.insert("a".to_owned(), order("a", OrderStatus::Pending));
        book.insert("b".to_owned(), order("b", OrderStatus::Traded));
        book.insert("c".to_owned(), order("c", OrderStatus::Confirmed));
        let (indexed, scanned) = ids_both_ways(&book);
        assert_eq!(indexed, ["a", "c"]);
        assert_eq!(indexed, scanned);
        if let Some(mut o) = book.get_mut("a") {
            o.status = OrderStatus::Cancelled;
        }
        assert_eq!(ids_both_ways(&book).0, ["c"]);
        book.clear();
        assert_eq!(book.active_values().count(), 0);
    }

    #[test]
    fn test_insert_counts_only_non_terminal_orders() {
        let mut book = OrderBook::with_capacity(4);
        book.insert("a".to_owned(), order("a", OrderStatus::Pending));
        book.insert("b".to_owned(), order("b", OrderStatus::Traded));
        assert_eq!(book.active_count(), 1);
        assert_eq!(book.len(), 2);
        assert!(!book.is_empty());
    }

    #[test]
    fn test_insert_replacing_an_order_corrects_the_count() {
        let mut book = OrderBook::with_capacity(4);
        book.insert("a".to_owned(), order("a", OrderStatus::Pending));
        let old = book.insert("a".to_owned(), order("a", OrderStatus::Cancelled));
        assert!(old.is_some());
        assert_eq!(book.active_count(), 0);
        book.insert("a".to_owned(), order("a", OrderStatus::Confirmed));
        assert_eq!(book.active_count(), 1);
    }

    #[test]
    fn test_get_mut_guard_corrects_the_count_on_drop() {
        let mut book = OrderBook::with_capacity(4);
        book.insert("a".to_owned(), order("a", OrderStatus::Pending));
        if let Some(mut o) = book.get_mut("a") {
            o.status = OrderStatus::Traded;
        }
        assert_eq!(book.active_count(), 0);
        if let Some(mut o) = book.get_mut("a") {
            o.status = OrderStatus::Triggered;
        }
        assert_eq!(book.active_count(), 1);
        // An unchanged handle leaves the count alone.
        assert!(book.get_mut("a").is_some());
        assert_eq!(book.active_count(), 1);
        assert!(book.get_mut("missing").is_none());
    }

    #[test]
    fn test_clear_resets_the_count() {
        let mut book = OrderBook::with_capacity(4);
        book.insert("a".to_owned(), order("a", OrderStatus::Pending));
        book.clear();
        assert_eq!(book.active_count(), 0);
        assert!(book.is_empty());
        assert!(!book.contains_key("a"));
        assert!(book.get("a").is_none());
        assert!(book.map().is_empty());
    }

    #[test]
    fn test_accessors_with_capacity_map_values_len_get_contains_key_is_empty() {
        let mut book = OrderBook::with_capacity(2);
        assert!(book.is_empty());
        book.insert("a".to_owned(), order("a", OrderStatus::Pending));
        book.insert("b".to_owned(), order("b", OrderStatus::Rejected));
        assert_eq!(book.len(), 2);
        assert_eq!(book.map().len(), 2);
        assert_eq!(book.values().count(), 2);
        assert!(book.contains_key("a"));
        assert_eq!(book.get("b").map(|o| o.status), Some(OrderStatus::Rejected));
        assert_eq!(book.active_count(), scan(&book));
    }

    proptest! {
        /// The O(1) count equals the old full scan after every step of a
        /// random sequence of inserts, replacements, status changes and
        /// clears.
        #[test]
        fn proptest_active_count_equals_the_scan(
            ops in proptest::collection::vec((0u8..4, 0usize..6, 0usize..10), 1..200)
        ) {
            let mut book = OrderBook::with_capacity(8);
            for (kind, key, status) in ops {
                let id = format!("o{key}");
                let st = ALL[status];
                match kind {
                    0 | 1 => { book.insert(id.clone(), order(&id, st)); }
                    2 => { if let Some(mut o) = book.get_mut(id.as_str()) { o.status = st; } }
                    _ => { if key == 0 { book.clear(); } }
                }
                prop_assert_eq!(book.active_count(), scan(&book));
                let (indexed, scanned) = ids_both_ways(&book);
                prop_assert_eq!(indexed, scanned);
            }
        }
    }
}
