//! The OMS order map with an O(1) count of active (non-terminal) orders.
//!
//! Audit plan PR6 (`active-plan-audit-fixes-2026-09-26.md`): the order runtime
//! asks `active_order_count()` once a second and after every order event and
//! mark batch, and that answer was a scan of every order held, terminal ones
//! included: O(orders), bounded by `MAX_TRACKED_ORDERS` (25,000).
//!
//! `OrderBook` keeps the count beside the map. Every way to change an order
//! goes through this type: `insert` and `clear` adjust the count directly, and
//! `get_mut` hands out an [`OrderMut`] guard that remembers whether the order
//! was active and corrects the count when the guard is dropped. So no call
//! site has to remember to update anything, and a new mutation site cannot
//! bypass the count without changing this file (the map is private here).
//!
//! # Complexity
//! `active_count` is O(1). `insert`, `get_mut` and the guard's drop are O(1)
//! average (one hash probe plus two `is_terminal` matches). `clear` is O(n) in
//! the map, as before. No allocation beyond what the map itself did.
//!
//! # Honest limits
//! - A guard leaked with `std::mem::forget` would skip its correction. Nothing
//!   in the engine does that; `OrderManagementSystem::active_order_count`
//!   cross-checks the count against the full scan in every debug build, so the
//!   whole test suite exercises it.
//! - The count is per map ENTRY, exactly as the old scan counted.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

use crate::oms::types::ManagedOrder;

/// The order map, keyed by order id, hashed with `ahash`.
pub(crate) type OrderMap = HashMap<String, ManagedOrder, ahash::RandomState>;

/// The OMS order map plus a running count of its non-terminal orders.
#[derive(Debug)]
pub(crate) struct OrderBook {
    map: OrderMap,
    active: usize,
}

impl OrderBook {
    /// An empty book pre-sized for `capacity` orders.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity_and_hasher(capacity, ahash::RandomState::new()),
            active: 0,
        }
    }

    /// The underlying map, read-only.
    pub(crate) fn map(&self) -> &OrderMap {
        &self.map
    }

    /// Number of non-terminal orders. O(1).
    pub(crate) fn active_count(&self) -> usize {
        self.active
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
    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        String: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(key)
    }

    /// The order held under `key`.
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&ManagedOrder>
    where
        String: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(key)
    }

    /// All orders, active and terminal.
    pub(crate) fn values(&self) -> std::collections::hash_map::Values<'_, String, ManagedOrder> {
        self.map.values()
    }

    /// Insert or replace the order under `key`, keeping the count exact.
    pub(crate) fn insert(&mut self, key: String, order: ManagedOrder) -> Option<ManagedOrder> {
        let adds = !order.is_terminal();
        let old = self.map.insert(key, order);
        if adds {
            self.active = self.active.saturating_add(1);
        }
        if let Some(prev) = old.as_ref()
            && !prev.is_terminal()
        {
            self.active = self.active.saturating_sub(1);
        }
        old
    }

    /// Remove every order. O(n) in the map.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.active = 0;
    }

    /// A mutable handle to the order under `key`. The count is corrected when
    /// the handle is dropped, whatever the caller changed.
    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<OrderMut<'_>>
    where
        String: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let Self { map, active } = self;
        map.get_mut(key).map(|order| OrderMut {
            was_active: !order.is_terminal(),
            order,
            active,
        })
    }
}

/// A mutable order handle that keeps [`OrderBook::active_count`] exact.
#[derive(Debug)]
pub(crate) struct OrderMut<'a> {
    order: &'a mut ManagedOrder,
    active: &'a mut usize,
    was_active: bool,
}

impl Deref for OrderMut<'_> {
    type Target = ManagedOrder;
    fn deref(&self) -> &ManagedOrder {
        self.order
    }
}

impl DerefMut for OrderMut<'_> {
    fn deref_mut(&mut self) -> &mut ManagedOrder {
        self.order
    }
}

impl Drop for OrderMut<'_> {
    fn drop(&mut self) {
        let is_active = !self.order.is_terminal();
        match (self.was_active, is_active) {
            (true, false) => *self.active = self.active.saturating_sub(1),
            (false, true) => *self.active = self.active.saturating_add(1),
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
            }
        }
    }
}
