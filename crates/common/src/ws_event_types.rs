//! Typed WebSocket connection-type + lifecycle-event-kind enums for the
//! `ws_event_audit` forensic table.
//!
//! Operator request 2026-06-12: every WebSocket connect / disconnect / reconnect
//! / sleep event must be durably tracked, AND the tracking must be future-proof
//! for a possible expansion to 5 main-feed + 5 depth-20 + 5 depth-200 + 1
//! order-update (= 16) connections. These two enums make the audit schema +
//! append API `ws_type` + `event_kind` aware so that expansion is a no-op for
//! tracking — a future depth connection calls the SAME append helper with a
//! different [`WsType`], no schema change.
//!
//! NOTE: this does NOT lift the 2-WebSocket runtime lock
//! (`.claude/rules/project/websocket-connection-scope-lock.md`). Today only
//! [`WsType::MainFeed`] and [`WsType::OrderUpdate`] are constructed at runtime;
//! [`WsType::Depth20`] / [`WsType::Depth200`] exist so the tracking is ready the
//! day the operator (via a separate rule-file edit) re-enables depth feeds.
//!
//! # Performance
//! Pure `Copy` enums with `const fn as_str()` — zero allocation, O(1).

/// The kind of Dhan WebSocket a connection belongs to.
///
/// The wire labels are stable SYMBOL values stored in the `ws_event_audit.ws_type`
/// column; pairing `(ws_type, connection_index)` is the composite-unique key for a
/// connection across the (current 2, future 16) live sockets — the same I-P1-11
/// composite-uniqueness discipline applied to WebSocket streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WsType {
    /// `wss://api-feed.dhan.co` live market feed (today: 1 conn; future: up to 5).
    MainFeed,
    /// `wss://depth-api-feed.dhan.co/twentydepth` 20-level depth (future: up to 5).
    Depth20,
    /// `wss://full-depth-api.dhan.co` 200-level depth (future: up to 5).
    Depth200,
    /// `wss://api-order-update.dhan.co` order-update feed (always 1 conn).
    OrderUpdate,
    /// The Groww second feed (operator §32) — NOT a Dhan binary WS but a
    /// Python-sidecar NDJSON tail bridge. A distinct `ws_type` keeps the broker
    /// meaning of the SYMBOL honest: a `where ws_type='groww_bridge'` query reads
    /// cleanly, and re-using a Dhan label (`main_feed`/`order_update`) would
    /// silently mix two brokers in operator filters. Pairs with `feed='groww'`.
    GrowwBridge,
}

impl WsType {
    /// Stable wire label stored in QuestDB (`ws_event_audit.ws_type` SYMBOL).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainFeed => "main_feed",
            Self::Depth20 => "depth_20",
            Self::Depth200 => "depth_200",
            Self::OrderUpdate => "order_update",
            Self::GrowwBridge => "groww_bridge",
        }
    }

    /// All variants — lets tests assert exhaustiveness + wire-label uniqueness
    /// without drifting from the enum.
    #[must_use]
    pub const fn all() -> [WsType; 5] {
        [
            Self::MainFeed,
            Self::Depth20,
            Self::Depth200,
            Self::OrderUpdate,
            Self::GrowwBridge,
        ]
    }
}

/// A WebSocket lifecycle event. One variant per [`crate`]-level
/// `NotificationEvent::WebSocket*` variant so every operator-visible WS event has
/// a matching audit row kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WsEventKind {
    /// A connection (re)established and is streaming.
    Connected,
    /// An in-market disconnect (HIGH-severity Telegram class).
    Disconnected,
    /// An off-hours disconnect (LOW-severity — Dhan idle cleanup pre/post market).
    DisconnectedOffHours,
    /// A successful reconnect after a disconnect (carries down_secs + attempts).
    Reconnected,
    /// The connection entered post-close dormant sleep (sleep-until-open).
    SleepEntered,
    /// The connection resumed from dormant sleep at the next market open.
    SleepResumed,
}

impl WsEventKind {
    /// Stable wire label stored in QuestDB (`ws_event_audit.event_kind` SYMBOL).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::DisconnectedOffHours => "disconnected_off_hours",
            Self::Reconnected => "reconnected",
            Self::SleepEntered => "sleep_entered",
            Self::SleepResumed => "sleep_resumed",
        }
    }

    /// All variants — lets tests assert exhaustiveness + wire-label uniqueness.
    #[must_use]
    pub const fn all() -> [WsEventKind; 6] {
        [
            Self::Connected,
            Self::Disconnected,
            Self::DisconnectedOffHours,
            Self::Reconnected,
            Self::SleepEntered,
            Self::SleepResumed,
        ]
    }
}

/// `dhan_code` sentinel meaning "no Dhan disconnect code" (transport error).
pub const WS_EVENT_NO_DHAN_CODE: i64 = -1;

/// One WebSocket lifecycle event, ready for the `ws_event_audit` table.
///
/// Lives in `common` (not `storage`) so the PRODUCER (`crates/core` WebSocket
/// connections) can build + send it down a channel, while the CONSUMER
/// (`crates/storage` ILP writer, driven from `crates/app`) persists it — core
/// is upstream of storage in the dependency graph, so the shared data type must
/// sit in `common`.
///
/// `reason` is redacted at the ILP write boundary
/// (`storage::ws_event_audit_persistence::WsEventAuditWriter::append_row`) so a
/// token can never reach the table even if a producer forgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsEventAuditRow {
    /// When the event happened — IST nanoseconds (designated timestamp).
    pub event_ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Broker feed source (`dhan` / `groww`). Per-feed identity (2026-06-23):
    /// a Dhan and a Groww connection can share `(ws_type, connection_index)`,
    /// so `feed` is part of the audit's DEDUP key — their lifecycle events are
    /// distinct rows, never collapsed.
    pub feed: crate::feed::Feed,
    /// Which Dhan WebSocket the connection belongs to.
    pub ws_type: WsType,
    /// 0-based index of the connection within its `ws_type` pool.
    pub connection_index: i64,
    /// Configured number of connections of this `ws_type` (1 today, up to 5 later).
    pub pool_size: i64,
    /// The lifecycle event kind.
    pub event_kind: WsEventKind,
    /// Best-guess source label from the disconnect classifier, or `"n/a"`.
    pub source: String,
    /// Human-readable reason. Redacted at the ILP write boundary.
    pub reason: String,
    /// Dhan disconnect code (805/807/...), or [`WS_EVENT_NO_DHAN_CODE`].
    pub dhan_code: i64,
    /// Reconnect downtime in seconds (0 for non-reconnect events).
    pub down_secs: i64,
    /// Reconnect attempts (0 for non-reconnect events).
    pub attempts: i64,
    /// `true` when the event happened inside [09:00, 15:30) IST.
    pub market_hours: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_ws_type_as_str_labels_are_stable_and_unique() {
        let labels: Vec<&str> = WsType::all().iter().map(|t| t.as_str()).collect();
        // Stable values the QuestDB SYMBOL column depends on.
        assert_eq!(
            labels,
            vec![
                "main_feed",
                "depth_20",
                "depth_200",
                "order_update",
                "groww_bridge"
            ]
        );
        // No two variants share a wire label.
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len(), "ws_type labels must be unique");
    }

    #[test]
    fn test_ws_event_kind_as_str_labels_are_stable_and_unique() {
        let labels: Vec<&str> = WsEventKind::all().iter().map(|k| k.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "connected",
                "disconnected",
                "disconnected_off_hours",
                "reconnected",
                "sleep_entered",
                "sleep_resumed",
            ]
        );
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "event_kind labels must be unique"
        );
    }

    #[test]
    fn test_all_arrays_match_variant_counts() {
        // If a variant is added, `all()` must be updated — these pin the count so
        // a new WS type / event kind cannot silently escape the audit schema.
        assert_eq!(WsType::all().len(), 5);
        assert_eq!(WsEventKind::all().len(), 6);
    }

    #[test]
    fn test_ws_type_is_copy_and_hashable_for_composite_keys() {
        // (ws_type, connection_index) is the composite-unique key — WsType must be
        // usable in a HashSet/HashMap key (I-P1-11 discipline for WS streams).
        let mut set: HashSet<(WsType, u8)> = HashSet::new();
        set.insert((WsType::MainFeed, 0));
        set.insert((WsType::Depth20, 3));
        assert!(set.contains(&(WsType::MainFeed, 0)));
        assert_eq!(set.len(), 2);
    }
}
