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
    /// The TrueData live-tick feed (feed #4, operator lock 2026-07-24) —
    /// the `wss://push.truedata.in` binary-tick market-data WebSocket. A
    /// distinct `ws_type` keeps the broker meaning of the SYMBOL honest:
    /// `where ws_type='truedata_feed'` reads cleanly and never mixes with
    /// a Dhan label. Pairs with `feed='truedata'`.
    TruedataFeed,
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
            Self::TruedataFeed => "truedata_feed",
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
            Self::TruedataFeed,
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
    /// The feed-agnostic sidecar stall watchdog killed + relaunched an
    /// alive-but-silent (or never-streamed) child (FEED-STALL-01 /
    /// FEED-STALL-01 §1b semantics). NOT an "up" kind and NOT a plain
    /// disconnect — the socket process was deliberately killed by OUR
    /// watchdog because the SERVER stopped delivering; the scoreboard maps
    /// it to the `stall_restart` / `never_streamed_restart` episode kinds
    /// (dual-feed scoreboard PR-B, 2026-07-10). The row's `source` carries
    /// a FIXED machine cause slug (`stall_silent_socket` /
    /// `stall_never_streamed` / `stall_auth_stale` / `stall_entitlement` —
    /// see `crate::feed_blame::STALL_SOURCE_*`), never raw child text.
    StallRestarted,
    /// The lane began dialing this connection.
    ///
    /// ADDED 2026-08-29. Every other kind presupposes a socket that OPENED,
    /// so a connection that failed every dial produced no row anywhere and
    /// was absent from the daily per-connection record — indistinguishable
    /// from a connection that was never planned. On 2026-08-12 the main feed
    /// failed twelve dials with `HTTP 400` and never handshook all session;
    /// it appeared in that record as nothing at all.
    ///
    /// This is the one kind that fires BEFORE the socket can deliver, so it
    /// establishes SET MEMBERSHIP and nothing else. It deliberately does NOT
    /// set `saw_any_event` in the rollup: that flag means "the socket did
    /// something", and a dial that failed is precisely the socket doing
    /// nothing.
    DialStarted,
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
            Self::StallRestarted => "stall_restarted",
            Self::DialStarted => "dial_started",
        }
    }

    /// All variants — lets tests assert exhaustiveness + wire-label uniqueness.
    #[must_use]
    pub const fn all() -> [WsEventKind; 8] {
        [
            Self::Connected,
            Self::Disconnected,
            Self::DisconnectedOffHours,
            Self::Reconnected,
            Self::SleepEntered,
            Self::SleepResumed,
            Self::StallRestarted,
            Self::DialStarted,
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
    /// Broker feed source (`dhan`). Per-feed identity (2026-06-23):
    /// two connections can share `(ws_type, connection_index)`,
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
                "truedata_feed"
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
                "stall_restarted",
                "dial_started",
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
        // 7 -> 8 on 2026-08-29 with `DialStarted`. This ratchet is why the
        // addition could not be silent: `all()` drives the persistence
        // round-trip test, so a kind missing from it would never have had its
        // ILP append exercised.
        assert_eq!(WsEventKind::all().len(), 8);
    }

    #[test]
    fn test_dial_started_is_not_an_up_kind_and_has_its_own_label() {
        // `dial_started` fires BEFORE the socket can deliver anything. Any
        // consumer that string-matches it as a connection-up signal would
        // report a socket healthy the instant it began dialing -- which is
        // the exact failure the kind was added to expose, inverted.
        let label = WsEventKind::DialStarted.as_str();
        assert_eq!(label, "dial_started");
        for reserved in [
            "connected",
            "reconnected",
            "sleep_resumed",
            "disconnected",
            "disconnected_off_hours",
            "stall_restarted",
        ] {
            assert_ne!(
                label, reserved,
                "dial_started must not collide with an existing lifecycle label"
            );
        }
        assert!(
            WsEventKind::all().contains(&WsEventKind::DialStarted),
            "a kind absent from all() never gets its ILP append exercised"
        );
    }

    #[test]
    fn test_stall_restarted_is_neither_up_kind_nor_plain_disconnect_label() {
        // Dual-feed scoreboard PR-B contract: the stall-restart lifecycle row
        // must never be mistaken for a connection-up signal or a plain
        // disconnect by string-matching consumers — its wire label is its
        // own distinct value.
        let label = WsEventKind::StallRestarted.as_str();
        assert_eq!(label, "stall_restarted");
        for reserved in [
            "connected",
            "reconnected",
            "sleep_resumed",
            "disconnected",
            "disconnected_off_hours",
        ] {
            assert_ne!(label, reserved);
        }
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
