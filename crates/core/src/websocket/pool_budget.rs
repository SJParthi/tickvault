//! Dhan connection-pool budget — PURE LOGIC, no I/O, no sockets, no tasks.
//!
//! Part of the 16-connection revival authorized by the operator on 2026-08-09
//! (`.claude/rules/project/websocket-connection-scope-lock.md`, section
//! "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS +
//! depth-20/depth-200 AUTHORIZED").
//!
//! # Why this exists — the failure it prevents
//! `15-live-market-feed.md:209` and `16-full-market-depth.md:183` say the same
//! thing, verbatim: "If more than 5 websockets are established, then the first
//! socket will be disconnected with `805` with every additional connection."
//! `20-annexure.md:148` glosses 805 as "Too many requests or connections —
//! further requests may result in blocking".
//!
//! Read that carefully: exceeding the cap does NOT reject the NEW socket. It
//! kills the FIRST-established — i.e. the oldest — one. So an accidental sixth
//! main-feed connection does not fail loudly where the mistake was made; it
//! silently destroys a healthy, fully-subscribed socket carrying up to 5,000
//! instruments, and the only evidence is a disconnect code on a connection
//! nobody touched. Worse, "with every additional connection" means a retry loop
//! would keep executing pool members one per attempt.
//!
//! The budget therefore refuses locally, BEFORE any dial. Fail-closed: better
//! fifteen connections and a loud refusal than sixteen and a silently murdered
//! pool member. This is the "Sixth socket opened → Pool budget refuses to open
//! it" row of the approved design's failure-mode table.
//!
//! # The authorized shape — INDIA FEED ONLY, exactly four endpoint types
//! | Endpoint | Host | Max conns | Instruments/conn |
//! |---|---|---|---|
//! | main-feed | `api-feed.dhan.co` (`15-live-market-feed.md:34`) | 5 | 5,000 (`:18`) |
//! | depth-20 | `depth-api-feed.dhan.co/twentydepth` (`16-full-market-depth.md:31`) | 5 | 50 (`:54`) |
//! | depth-200 | `full-depth-api.dhan.co/twohundreddepth` (`16-full-market-depth.md:39`) | 5 | 1 (`:77`) |
//! | order-update | `api-order-update.dhan.co` (`17-live-order-update.md:27`) | 1 | n/a |
//! | **total** | | **16** | |
//!
//! **2026-09-26 — a second, depth-only account.** The operator added a second
//! Dhan account in the operator's own name, used ONLY for extra depth-20 and
//! depth-200 sockets (`websocket-connection-scope-lock.md` § "2026-09-26 — A
//! SECOND DHAN ACCOUNT"). Dhan counts every cap per Client ID, so that account
//! adds its own 5 + 5 and never a main-feed or order-update socket: 16 + 10 =
//! 26 slots in total. See [`DhanAccount`] for the slot layout. The account
//! ships OFF, so the live count stays at 16 until it is switched on.
//!
//! **`global-stocks-api-feed.dhan.co` (`25-global-stocks.md:544`) — the US
//! global-stocks feed — is a FIFTH endpoint type on a different host and is
//! FORBIDDEN.** Operator constraint, 2026-08-09: India feed only, never the US
//! global-stocks feed. [`DhanEndpointType`] deliberately has no variant for it,
//! which makes the scope lock a compile-time property: there is no value a
//! caller could construct to ask this budget for a global-stocks connection.
//! Its documented behaviour also differs (6 concurrent connections per
//! `clientId`, `25-global-stocks.md:556`), so silently folding it in would have
//! imported the wrong cap as well as the wrong scope.
//!
//! **Honest sourcing of the per-type independence.** The docs pack states the
//! 5-connection cap separately in the main-feed and depth chapters but never
//! says in so many words that the caps are counted independently per endpoint
//! type. That independence — which is what makes 5 + 5 + 5 + 1 = 16 legal
//! rather than 5 total — rests on Dhan's support confirmation of 2026-04-06 as
//! recorded in `websocket-connection-scope-lock.md`, NOT on the doc pack. It is
//! recorded here as such rather than dressed up as a documented fact. If that
//! confirmation is ever contradicted live, this budget is the single place that
//! changes, and it fails closed in the meantime.
//!
//! # Observability of a refusal
//! A refusal returns a typed [`PoolBudgetRefusal`] AND increments
//! [`POOL_BUDGET_REFUSED_METRIC`] AND logs at `warn!`. It deliberately does NOT
//! log at `error!`: coded `error!` on the Dhan surface is the paging tier, and
//! `dhan-rest-only-noise-lock-2026-07-14.md` §2 fixes the Dhan Telegram alert
//! family at four items. Adding a fifth needs its own dated operator quote, so
//! this refusal is counter-and-log visible rather than a page.
//!
//! # Allocation
//! [`PoolBudget`] is six `u8` counters on the stack; every method is integer
//! comparison and saturating arithmetic. No heap, no locks, no indexing.

use tracing::warn;

use tickvault_common::constants::{
    MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION,
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, MAX_TWENTY_DEPTH_CONNECTIONS,
    MAX_TWO_HUNDRED_DEPTH_CONNECTIONS, MAX_WEBSOCKET_CONNECTIONS,
};

use super::types::ConnectionId;

// ---------------------------------------------------------------------------
// Connection caps
// ---------------------------------------------------------------------------

/// Max simultaneous main-feed connections. `15-live-market-feed.md:18`: "You
/// can establish upto five WebSocket connections per user".
/// Raised from 1 to 5 by the 2026-08-09 operator quote.
pub const MAX_MAIN_FEED_CONNECTIONS: u8 = MAX_WEBSOCKET_CONNECTIONS as u8;

/// Max simultaneous depth-20 connections. `16-full-market-depth.md:183` states
/// the same 5-socket / `805` rule as the main feed.
/// FORBIDDEN by our own scope lock before 2026-08-09.
pub const MAX_DEPTH_20_CONNECTIONS: u8 = MAX_TWENTY_DEPTH_CONNECTIONS as u8;

/// Max simultaneous depth-200 connections (`16-full-market-depth.md:183`).
/// FORBIDDEN by our own scope lock before 2026-08-09.
pub const MAX_DEPTH_200_CONNECTIONS: u8 = MAX_TWO_HUNDRED_DEPTH_CONNECTIONS as u8;

/// Max simultaneous order-update connections
/// (`17-live-order-update.md:27`). Unchanged at 1 — the 2026-08-09 quote is
/// a MARKET-DATA authorization; live order fire stays locked.
pub const MAX_ORDER_UPDATE_CONNECTIONS: u8 = 1;

/// Connections the PRIMARY account may hold: the sum of the four per-type caps,
/// 5 + 5 + 5 + 1 = 16. Pinned by
/// `test_max_connections_per_type_sums_to_the_total_ceiling`, which is what
/// makes a seventeenth primary connection arithmetically unreachable rather
/// than merely unlikely.
pub const MAX_PRIMARY_ACCOUNT_CONNECTIONS: u8 = MAX_MAIN_FEED_CONNECTIONS
    + MAX_DEPTH_20_CONNECTIONS
    + MAX_DEPTH_200_CONNECTIONS
    + MAX_ORDER_UPDATE_CONNECTIONS;

/// Max depth-20 connections on the second, DEPTH-only account
/// (`websocket-connection-scope-lock.md` § "2026-09-26 — A SECOND DHAN
/// ACCOUNT"). Dhan's limits are per Client ID, so this account has its own five.
pub const MAX_DEPTH_ACCOUNT_DEPTH_20_CONNECTIONS: u8 = MAX_DEPTH_20_CONNECTIONS;

/// Max depth-200 connections on the DEPTH account (same section, same reason).
pub const MAX_DEPTH_ACCOUNT_DEPTH_200_CONNECTIONS: u8 = MAX_DEPTH_200_CONNECTIONS;

/// Connections the DEPTH account may hold: 5 depth-20 + 5 depth-200. It never
/// opens a main-feed or order-update socket.
pub const MAX_DEPTH_ACCOUNT_CONNECTIONS: u8 =
    MAX_DEPTH_ACCOUNT_DEPTH_20_CONNECTIONS + MAX_DEPTH_ACCOUNT_DEPTH_200_CONNECTIONS;

/// Hard ceiling across every account and every endpoint type: 16 primary + 10
/// depth-account = 26 (2026-09-26). The depth account ships OFF, so in
/// production today the live count stays at 16; this is the size of the slot
/// space, which every per-socket table is dimensioned from.
pub const MAX_TOTAL_DHAN_CONNECTIONS: u8 =
    MAX_PRIMARY_ACCOUNT_CONNECTIONS + MAX_DEPTH_ACCOUNT_CONNECTIONS;

/// The metric label for every global socket slot, resolved at compile time.
///
/// A `"connection"` label is attached to per-socket series from the pool
/// supervisor (instruments held) and the frame drain (tick age, frames). The
/// path that publishes them is cold, but `crates/core/src/websocket/` is
/// scanned as hot path by the banned-pattern gate, and a `to_string()` there
/// is refused on principle: a label set that is bounded and known at compile
/// time has no reason to allocate at all.
pub const CONNECTION_SLOT_LABELS: [&str; MAX_TOTAL_DHAN_CONNECTIONS as usize] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23", "24", "25",
];

/// The `"connection"` label for a global slot; `"unknown"` past the ceiling,
/// which the planner cannot produce but a label lookup must never panic on.
#[must_use]
pub const fn connection_slot_label(global_index: u8) -> &'static str {
    let index = global_index as usize;
    if index < CONNECTION_SLOT_LABELS.len() {
        CONNECTION_SLOT_LABELS[index]
    } else {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// Instrument caps (Dhan-side, per connection)
// ---------------------------------------------------------------------------

/// Instruments a single main-feed connection may carry.
/// `15-live-market-feed.md:18`: "You can establish upto five WebSocket
/// connections per user with 5000 instruments on each connection."
pub const MAIN_FEED_INSTRUMENTS_PER_CONNECTION: u32 =
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION as u32;

/// Instruments a single depth-20 connection may carry.
/// `16-full-market-depth.md:54`: "For 20 Level Market Depth, you can subscribe
/// upto 50 instruments in a single connection".
pub const DEPTH_20_INSTRUMENTS_PER_CONNECTION: u32 =
    MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION as u32;

/// Instruments a single depth-200 connection may carry.
/// `16-full-market-depth.md:77`: "In 200 level market depth, only 1 instrument
/// per connection can be subscribed." The 200-level book is a whole
/// connection's worth of bandwidth on its own.
pub const DEPTH_200_INSTRUMENTS_PER_CONNECTION: u32 =
    MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION as u32;

/// The order-update socket carries no instrument subscriptions at all; it
/// receives events for orders we placed (`17-live-order-update.md`).
pub const ORDER_UPDATE_INSTRUMENTS_PER_CONNECTION: u32 = 0;

/// Max instruments in a single main-feed JSON subscribe message.
/// `15-live-market-feed.md:50`: "You can only send upto 100 instruments in a
/// single JSON message. You can send multiple messages over a single connection
/// to subscribe to all instruments".
pub const MAIN_FEED_INSTRUMENTS_PER_SUBSCRIBE_MESSAGE: u32 = 100;

/// Max instruments in a single depth-20 JSON subscribe message.
///
/// NOTE the asymmetry with the main feed — `16-full-market-depth.md:58`: "You
/// can send all 50 instruments in a single JSON message for 20 Depth." The
/// depth-20 batch limit is its whole per-connection capacity, so applying the
/// main feed's 100 here would be wrong in one direction and applying a blanket
/// 100 to both would be wrong in the other. Per-endpoint, not global.
pub const DEPTH_20_INSTRUMENTS_PER_SUBSCRIBE_MESSAGE: u32 = 50;

// ---------------------------------------------------------------------------
// Endpoint hosts — NOT redefined here (INDIA feed only)
// ---------------------------------------------------------------------------
//
// This module deliberately declares NO WebSocket URL constants. Every host
// already has exactly one home and duplicating them here would be wrong three
// times over:
//
//  1. `crates/common/src/constants.rs` is the single WS-URL source (its own
//     `GROWW_SOCKET_URL` carries the comment "constants.rs is the single
//     WS-URL source"). The depth hosts live there as
//     `DHAN_TWENTY_DEPTH_WS_BASE_URL` and `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL`.
//
//  2. The main-feed and order-update URLs are CONFIG-driven, not constants —
//     `DhanConfig::websocket_url` / `DhanConfig::order_update_websocket_url`.
//     Hardcoding them here would re-introduce exactly the hardcoded-value class
//     the banned-pattern scanner exists to stop.
//
//  3. Most importantly, for depth-200 the docs pack and our tree DISAGREE, and
//     the tree is right. `16-full-market-depth.md:39` gives
//     `wss://full-depth-api.dhan.co/twohundreddepth`, but
//     `constants.rs:1550-1561` records that on 2026-04-23 Parthiban verified
//     against Dhan's own vendor SDK that the ROOT path
//     (`wss://full-depth-api.dhan.co`) is what actually works for our account,
//     after `/twohundreddepth` returned `ResetWithoutClosingHandshake` for two
//     solid weeks — a live finding that REVERSED Dhan's own ticket #5519522.
//     Copying the documented path into a new constant here would silently
//     regress that fix. The connection layer must take the depth-200 URL from
//     `constants.rs`, not from the docs pack.
//
// India-feed scope is instead enforced structurally: [`DhanEndpointType`] has
// exactly four variants, none of which is the US global-stocks feed
// (`wss://global-stocks-api-feed.dhan.co`, `25-global-stocks.md:544`, which
// also carries a DIFFERENT cap of 6 connections per clientId at
// `25-global-stocks.md:556`). There is no value a caller can construct to ask
// this budget for a global-stocks connection, and
// `test_pool_budget_can_never_reach_the_us_global_stocks_endpoint` scans this
// module's own source to keep it that way.

/// Metric incremented on every refused connection open.
/// Labels: `account` (`primary` or `depth`), `endpoint` (the endpoint type),
/// `reason` (`endpoint_at_capacity`, `total_at_capacity` or
/// `endpoint_not_permitted`). Local `/metrics` only — not EMF-selected.
pub const POOL_BUDGET_REFUSED_METRIC: &str = "tv_dhan_pool_budget_refused_total";

// ---------------------------------------------------------------------------
// Endpoint type
// ---------------------------------------------------------------------------

/// The four — and only four — Dhan WebSocket endpoint types this product may
/// ever open. Any additional endpoint stays FORBIDDEN by the scope lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DhanEndpointType {
    /// Live market-data feed: ticker / quote / full packets.
    MainFeed,
    /// 20-level market depth.
    Depth20,
    /// 200-level full market depth.
    Depth200,
    /// Order/trade lifecycle events for orders we placed.
    OrderUpdate,
}

impl DhanEndpointType {
    /// Every endpoint type, in jitter-slot order.
    pub const ALL: [Self; 4] = [
        Self::MainFeed,
        Self::Depth20,
        Self::Depth200,
        Self::OrderUpdate,
    ];

    /// Max simultaneous connections this endpoint type permits.
    #[must_use]
    pub const fn max_connections(self) -> u8 {
        match self {
            Self::MainFeed => MAX_MAIN_FEED_CONNECTIONS,
            Self::Depth20 => MAX_DEPTH_20_CONNECTIONS,
            Self::Depth200 => MAX_DEPTH_200_CONNECTIONS,
            Self::OrderUpdate => MAX_ORDER_UPDATE_CONNECTIONS,
        }
    }

    /// The byte this endpoint writes into a TVW4 WAL record (2026-09-02).
    ///
    /// ONE mapping, here, so the sink that captures a frame and the refold that
    /// replays it can never disagree about which parser the frame belongs to.
    /// Distinct per variant — pinned by
    /// `every_endpoint_type_maps_to_a_distinct_wal_endpoint`.
    #[must_use]
    pub const fn wal_endpoint(&self) -> tickvault_storage::ws_frame_spill::WalEndpoint {
        use tickvault_storage::ws_frame_spill::WalEndpoint;
        match self {
            Self::MainFeed => WalEndpoint::MainFeed,
            Self::Depth20 => WalEndpoint::Depth20,
            Self::Depth200 => WalEndpoint::Depth200,
            Self::OrderUpdate => WalEndpoint::OrderUpdate,
        }
    }

    /// Does this endpoint need a CLIENT-originated keepalive ping?
    ///
    /// # The measurement (prod, 2026-08-26, market open)
    ///
    /// `tv_dhan_ws_control_frames_total{kind="ping"}`:
    ///
    /// | endpoint | server pings received |
    /// |---|---|
    /// | `main_feed` | 3,460 |
    /// | `depth_20` | 12,110 |
    /// | **`depth_200`** | **series absent entirely — ZERO** |
    ///
    /// The counter is created lazily on first ping, so an absent series means
    /// depth-200 received **no control frame at all** in a whole session.
    /// `docs/dhan-ref/full-market-depth.md:107` claims *"Server pings every 10
    /// seconds — same ping-pong mechanism as Live Market Feed."* **That claim
    /// is false for this endpoint.**
    ///
    /// # Why it matters, and why the fix is a ping rather than a bigger timeout
    ///
    /// `IdleWatchdog`'s own header states its purpose: *"this timer is not
    /// merely 'is the server quiet?' — it is also, and mostly, **'have WE
    /// stopped draining?'**"*. That works because Dhan pings, our library
    /// auto-pongs **only while the read loop polls**, and the arriving traffic
    /// therefore proves we are draining.
    ///
    /// With no server ping there is nothing to pong, so on depth-200 the only
    /// activity is market data — and the watchdog silently degrades from
    /// measuring OUR health to measuring whether an ILLIQUID OPTION HAPPENS TO
    /// BE TRADING. The five depth-200 instruments are FINNIFTY / MIDCPNIFTY /
    /// NIFTY options, which legitimately go 27+ seconds without a book update.
    ///
    /// Measured consequence, same session: `main_feed` and `order_update` took
    /// **0** disconnects while `depth_200` took **265**, every one of them
    /// self-inflicted at `idle_secs: 27`. Three of the five sockets cycled 19
    /// times each and then stopped dead at **09:14:45** — nothing was fixed at
    /// 09:15; the market opened.
    ///
    /// A client ping restores the intended semantics exactly: we ping, Dhan
    /// pongs, and the pong arrives **only if the socket is alive AND our read
    /// loop is draining** — which is the question the watchdog was built to
    /// ask. Merely raising the timeout would silence the symptom while leaving
    /// the watchdog measuring liquidity.
    ///
    /// Deliberately NOT applied to the endpoints Dhan already pings: their
    /// 2026-08-19 repair works, proven by main_feed going from 2,150
    /// disconnects/day to 0, and adding redundant traffic to a working path
    /// buys nothing.
    #[must_use]
    pub const fn needs_client_keepalive_ping(self) -> bool {
        match self {
            // Measured: zero server pings, ever.
            Self::Depth200 => true,
            // Measured: Dhan pings these, and the 2026-08-19 fix already turns
            // that ping into a watchdog reset.
            Self::MainFeed | Self::Depth20 | Self::OrderUpdate => false,
        }
    }

    /// Max instruments a single connection of this type may subscribe.
    #[must_use]
    pub const fn max_instruments_per_connection(self) -> u32 {
        match self {
            Self::MainFeed => MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
            Self::Depth20 => DEPTH_20_INSTRUMENTS_PER_CONNECTION,
            Self::Depth200 => DEPTH_200_INSTRUMENTS_PER_CONNECTION,
            Self::OrderUpdate => ORDER_UPDATE_INSTRUMENTS_PER_CONNECTION,
        }
    }

    /// Total instruments this endpoint type can carry across its whole
    /// authorized pool — `max_connections × max_instruments_per_connection`.
    ///
    /// Exists so callers sizing a subscription set never multiply those two
    /// numbers themselves. A set larger than this makes `plan_pool` refuse the
    /// ENTIRE pool, not just the excess, so an off-by-one in a caller's own
    /// arithmetic takes the whole endpoint down rather than degrading it.
    #[must_use]
    pub const fn subscription_capacity(self) -> usize {
        self.max_connections() as usize * self.max_instruments_per_connection() as usize
    }

    /// Max instruments this endpoint type accepts in ONE JSON subscribe
    /// message. A larger set is dispatched as several sequential messages on
    /// the same connection.
    ///
    /// Deliberately per-endpoint rather than one global constant: the main feed
    /// caps a message at 100 (`15-live-market-feed.md:50`) while depth-20
    /// explicitly permits all 50 of its instruments in a single message
    /// (`16-full-market-depth.md:58`). A single blanket value would be wrong
    /// for one of them whichever number was chosen.
    #[must_use]
    pub const fn max_instruments_per_subscribe_message(self) -> u32 {
        match self {
            Self::MainFeed => MAIN_FEED_INSTRUMENTS_PER_SUBSCRIBE_MESSAGE,
            Self::Depth20 => DEPTH_20_INSTRUMENTS_PER_SUBSCRIBE_MESSAGE,
            Self::Depth200 => DEPTH_200_INSTRUMENTS_PER_CONNECTION,
            Self::OrderUpdate => ORDER_UPDATE_INSTRUMENTS_PER_CONNECTION,
        }
    }

    /// First global connection index owned by this endpoint type.
    ///
    /// The PRIMARY account's slots. The four types tile `0..16` contiguously and
    /// without overlap (`main-feed 0..5`, `depth-20 5..10`, `depth-200 10..15`,
    /// `order-update 15..16`), so every live connection has a unique global
    /// index and therefore — via
    /// [`super::reconnect_ladder::reconnect_jitter_ms`] — a unique reconnect
    /// stagger. Pinned by `test_jitter_base_tiles_the_sixteen_slots_exactly`. The
    /// depth account's pools sit after these, at `16..26` — see
    /// [`DhanAccount::jitter_base`].
    #[must_use]
    pub const fn jitter_base(self) -> u8 {
        match self {
            Self::MainFeed => 0,
            Self::Depth20 => MAX_MAIN_FEED_CONNECTIONS,
            Self::Depth200 => MAX_MAIN_FEED_CONNECTIONS + MAX_DEPTH_20_CONNECTIONS,
            Self::OrderUpdate => {
                MAX_MAIN_FEED_CONNECTIONS + MAX_DEPTH_20_CONNECTIONS + MAX_DEPTH_200_CONNECTIONS
            }
        }
    }

    /// Stable lowercase tag for logs and metric labels. `'static` so it can be
    /// used as a metric label without allocating.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainFeed => "main_feed",
            Self::Depth20 => "depth_20",
            Self::Depth200 => "depth_200",
            Self::OrderUpdate => "order_update",
        }
    }
}

impl core::fmt::Display for DhanEndpointType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// Which Dhan account a socket is opened on (2026-09-26).
///
/// Dhan counts connections per Client ID, and an over-limit socket is not
/// refused: the OLDEST one of that type on that account is killed with 805. So
/// every cap in this module is per account, and the two accounts' sockets live
/// in disjoint slices of the global slot space:
///
/// | account | endpoint | global slots |
/// |---|---|---|
/// | primary | main feed | 0..5 |
/// | primary | depth-20 | 5..10 |
/// | primary | depth-200 | 10..15 |
/// | primary | order update | 15 |
/// | depth | depth-20 | 16..21 |
/// | depth | depth-200 | 21..26 |
///
/// The primary block is exactly the pre-2026-09-26 layout, so a socket on the
/// primary account keeps its slot, its metric label and its reconnect stagger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DhanAccount {
    /// The operator's trading account: main feed, depth, order updates.
    Primary,
    /// The second account, in the operator's own name, used ONLY for extra
    /// depth-20 and depth-200 sockets. It never opens a main-feed or
    /// order-update socket and never places an order.
    Depth,
}

impl DhanAccount {
    /// Both accounts, in slot order.
    pub const ALL: [Self; 2] = [Self::Primary, Self::Depth];

    /// Whether this account may open `endpoint` at all.
    #[must_use]
    pub const fn permits(self, endpoint: DhanEndpointType) -> bool {
        match self {
            Self::Primary => true,
            Self::Depth => matches!(
                endpoint,
                DhanEndpointType::Depth20 | DhanEndpointType::Depth200
            ),
        }
    }

    /// Max simultaneous `endpoint` connections on this account; `0` for an
    /// endpoint the account may not open.
    #[must_use]
    pub const fn max_connections(self, endpoint: DhanEndpointType) -> u8 {
        match (self, endpoint) {
            (Self::Primary, _) => endpoint.max_connections(),
            (Self::Depth, DhanEndpointType::Depth20) => MAX_DEPTH_ACCOUNT_DEPTH_20_CONNECTIONS,
            (Self::Depth, DhanEndpointType::Depth200) => MAX_DEPTH_ACCOUNT_DEPTH_200_CONNECTIONS,
            (Self::Depth, DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate) => 0,
        }
    }

    /// Every connection this account may hold at once.
    #[must_use]
    pub const fn connection_ceiling(self) -> u8 {
        match self {
            Self::Primary => MAX_PRIMARY_ACCOUNT_CONNECTIONS,
            Self::Depth => MAX_DEPTH_ACCOUNT_CONNECTIONS,
        }
    }

    /// First global slot of this account's `endpoint` pool. For an endpoint
    /// the account may not open this is the ceiling itself, so the pool's
    /// range `base..base + 0` is empty and names no slot.
    #[must_use]
    pub const fn jitter_base(self, endpoint: DhanEndpointType) -> u8 {
        match (self, endpoint) {
            (Self::Primary, _) => endpoint.jitter_base(),
            (Self::Depth, DhanEndpointType::Depth20) => MAX_PRIMARY_ACCOUNT_CONNECTIONS,
            (Self::Depth, DhanEndpointType::Depth200) => {
                MAX_PRIMARY_ACCOUNT_CONNECTIONS + MAX_DEPTH_ACCOUNT_DEPTH_20_CONNECTIONS
            }
            (Self::Depth, DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate) => {
                MAX_TOTAL_DHAN_CONNECTIONS
            }
        }
    }

    /// Stable lowercase tag for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Depth => "depth",
        }
    }
}

impl core::fmt::Display for DhanAccount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The account and endpoint that own a global slot; `None` past the ceiling.
///
/// Eight range tests at most (two accounts × four endpoints), no table to keep
/// in step with the pool: the ranges come from the same `jitter_base` and
/// `max_connections` the budget grants slots from.
#[must_use]
pub const fn slot_owner(global_index: ConnectionId) -> Option<(DhanAccount, DhanEndpointType)> {
    let mut a = 0;
    while a < DhanAccount::ALL.len() {
        let account = DhanAccount::ALL[a];
        let mut e = 0;
        while e < DhanEndpointType::ALL.len() {
            let endpoint = DhanEndpointType::ALL[e];
            let start = account.jitter_base(endpoint);
            let end = start.saturating_add(account.max_connections(endpoint));
            if global_index >= start && global_index < end {
                return Some((account, endpoint));
            }
            e += 1;
        }
        a += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Refusal
// ---------------------------------------------------------------------------

/// Why a connection open was refused locally.
///
/// A refusal is ALWAYS the correct outcome, never an error to be retried
/// around: opening anyway would make Dhan silently kill an older, healthy,
/// fully-subscribed socket with code 805.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PoolBudgetRefusal {
    /// This endpoint type already holds its maximum connections on this
    /// account.
    #[error(
        "refused to open a {endpoint} connection on the {account} account: {open}/{max} already \
         open for this endpoint type; opening a further one would make Dhan silently disconnect \
         the OLDEST with code 805"
    )]
    EndpointTypeAtCapacity {
        /// The account whose pool is full.
        account: DhanAccount,
        /// The endpoint type that is full.
        endpoint: DhanEndpointType,
        /// Connections currently open for that type on that account.
        open: u8,
        /// The type's cap on that account.
        max: u8,
    },
    /// The account's connection ceiling is already reached (16 primary, 10
    /// depth).
    #[error(
        "refused to open a {endpoint} connection on the {account} account: {open}/{max} total \
         connections already open on that account (the operator-authorized ceiling)"
    )]
    TotalAtCapacity {
        /// The account whose ceiling is reached.
        account: DhanAccount,
        /// The endpoint type that was requested.
        endpoint: DhanEndpointType,
        /// Total connections currently open on that account.
        open: u16,
        /// That account's ceiling.
        max: u16,
    },
    /// The account may never open this endpoint type: the depth account opens
    /// depth sockets only (`websocket-connection-scope-lock.md` § 2026-09-26).
    #[error(
        "refused to open a {endpoint} connection on the {account} account: that account is \
         authorized for depth-20 and depth-200 sockets only"
    )]
    EndpointNotPermittedOnAccount {
        /// The account that was asked.
        account: DhanAccount,
        /// The endpoint type it may not open.
        endpoint: DhanEndpointType,
    },
}

impl PoolBudgetRefusal {
    /// Short `'static` reason tag, for metric labels.
    #[must_use]
    pub const fn reason_str(self) -> &'static str {
        match self {
            Self::EndpointTypeAtCapacity { .. } => "endpoint_at_capacity",
            Self::TotalAtCapacity { .. } => "total_at_capacity",
            Self::EndpointNotPermittedOnAccount { .. } => "endpoint_not_permitted",
        }
    }

    /// The endpoint type whose open was refused.
    #[must_use]
    pub const fn endpoint(self) -> DhanEndpointType {
        match self {
            Self::EndpointTypeAtCapacity { endpoint, .. }
            | Self::TotalAtCapacity { endpoint, .. }
            | Self::EndpointNotPermittedOnAccount { endpoint, .. } => endpoint,
        }
    }

    /// The account whose open was refused.
    #[must_use]
    pub const fn account(self) -> DhanAccount {
        match self {
            Self::EndpointTypeAtCapacity { account, .. }
            | Self::TotalAtCapacity { account, .. }
            | Self::EndpointNotPermittedOnAccount { account, .. } => account,
        }
    }
}

// ---------------------------------------------------------------------------
// Granted slot
// ---------------------------------------------------------------------------

/// Proof that the budget granted one connection. Carries the indices the
/// connection task needs; holding one is what entitles a caller to dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSlot {
    /// Which account this slot's socket is opened on.
    pub account: DhanAccount,
    /// Which endpoint type this slot belongs to.
    pub endpoint: DhanEndpointType,
    /// Zero-based index of this connection WITHIN its own pool (`0..max`).
    pub pool_index: u8,
    /// Zero-based index of this connection across ALL pools of BOTH accounts
    /// (`0..MAX_TOTAL_DHAN_CONNECTIONS`).
    /// Feeds [`super::reconnect_ladder::reconnect_jitter_ms`].
    pub global_index: ConnectionId,
}

// ---------------------------------------------------------------------------
// The budget
// ---------------------------------------------------------------------------

/// Per-endpoint-type connection accounting with a fail-closed global ceiling.
///
/// Not thread-safe by itself — it is a plain value. The wiring round owns
/// whether it lives behind a mutex on a supervisor task or is confined to a
/// single owner; keeping it a plain value is what makes it exhaustively
/// testable here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolBudget {
    main_feed: u8,
    depth_20: u8,
    depth_200: u8,
    order_update: u8,
    /// Depth-20 sockets open on the DEPTH account.
    depth_account_depth_20: u8,
    /// Depth-200 sockets open on the DEPTH account.
    depth_account_depth_200: u8,
}

impl PoolBudget {
    /// An empty budget: nothing open.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Connections currently open for `endpoint` on the PRIMARY account.
    #[must_use]
    pub const fn open_count(&self, endpoint: DhanEndpointType) -> u8 {
        self.open_count_on(DhanAccount::Primary, endpoint)
    }

    /// Connections currently open for `endpoint` on `account`. Always `0` for
    /// an endpoint the account may not open.
    #[must_use]
    pub const fn open_count_on(&self, account: DhanAccount, endpoint: DhanEndpointType) -> u8 {
        match (account, endpoint) {
            (DhanAccount::Primary, DhanEndpointType::MainFeed) => self.main_feed,
            (DhanAccount::Primary, DhanEndpointType::Depth20) => self.depth_20,
            (DhanAccount::Primary, DhanEndpointType::Depth200) => self.depth_200,
            (DhanAccount::Primary, DhanEndpointType::OrderUpdate) => self.order_update,
            (DhanAccount::Depth, DhanEndpointType::Depth20) => self.depth_account_depth_20,
            (DhanAccount::Depth, DhanEndpointType::Depth200) => self.depth_account_depth_200,
            (DhanAccount::Depth, DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate) => 0,
        }
    }

    /// Total connections currently open on `account`.
    #[must_use]
    pub const fn total_open_on(&self, account: DhanAccount) -> u16 {
        match account {
            DhanAccount::Primary => {
                (self.main_feed as u16)
                    + (self.depth_20 as u16)
                    + (self.depth_200 as u16)
                    + (self.order_update as u16)
            }
            DhanAccount::Depth => {
                (self.depth_account_depth_20 as u16) + (self.depth_account_depth_200 as u16)
            }
        }
    }

    /// Total connections currently open across every account and endpoint type.
    ///
    /// `u16` so the sum is representable even if a future cap edit pushes the
    /// total past 255 — the arithmetic can never wrap into a falsely-small
    /// total that would let the ceiling check pass.
    #[must_use]
    pub const fn total_open(&self) -> u16 {
        self.total_open_on(DhanAccount::Primary) + self.total_open_on(DhanAccount::Depth)
    }

    /// Attempts to reserve one PRIMARY-account connection of `endpoint`.
    ///
    /// # Errors
    /// As [`Self::try_open_on`].
    pub fn try_open(
        &mut self,
        endpoint: DhanEndpointType,
    ) -> Result<ConnectionSlot, PoolBudgetRefusal> {
        self.try_open_on(DhanAccount::Primary, endpoint)
    }

    /// Attempts to reserve one connection of `endpoint` on `account`.
    ///
    /// Checks that the account may open the endpoint at all, then the
    /// account's ceiling, then the per-type cap, and only then mutates. On
    /// refusal NOTHING is mutated, the typed refusal is returned,
    /// [`POOL_BUDGET_REFUSED_METRIC`] is incremented and a `warn!` is logged.
    ///
    /// # Errors
    /// [`PoolBudgetRefusal::EndpointNotPermittedOnAccount`] for a main-feed or
    /// order-update socket on the depth account;
    /// [`PoolBudgetRefusal::TotalAtCapacity`] when the account's ceiling is
    /// reached; [`PoolBudgetRefusal::EndpointTypeAtCapacity`] when the endpoint
    /// type is full on that account.
    pub fn try_open_on(
        &mut self,
        account: DhanAccount,
        endpoint: DhanEndpointType,
    ) -> Result<ConnectionSlot, PoolBudgetRefusal> {
        if !account.permits(endpoint) {
            return Err(
                self.refuse(PoolBudgetRefusal::EndpointNotPermittedOnAccount { account, endpoint })
            );
        }

        let total = self.total_open_on(account);
        let ceiling = u16::from(account.connection_ceiling());
        if total >= ceiling {
            return Err(self.refuse(PoolBudgetRefusal::TotalAtCapacity {
                account,
                endpoint,
                open: total,
                max: ceiling,
            }));
        }

        let open = self.open_count_on(account, endpoint);
        let max = account.max_connections(endpoint);
        if open >= max {
            return Err(self.refuse(PoolBudgetRefusal::EndpointTypeAtCapacity {
                account,
                endpoint,
                open,
                max,
            }));
        }

        self.set_open_count(account, endpoint, open.saturating_add(1));

        Ok(ConnectionSlot {
            account,
            endpoint,
            pool_index: open,
            global_index: account.jitter_base(endpoint).saturating_add(open),
        })
    }

    /// Returns one PRIMARY-account connection of `endpoint` to the budget.
    pub fn release(&mut self, endpoint: DhanEndpointType) {
        self.release_on(DhanAccount::Primary, endpoint);
    }

    /// Returns one connection of `endpoint` on `account` to the budget.
    ///
    /// Saturates at zero, so a double-release can never underflow into a huge
    /// count that would then let the budget over-open. A release of a type with
    /// nothing open — or one the account may not open — is a no-op.
    pub fn release_on(&mut self, account: DhanAccount, endpoint: DhanEndpointType) {
        let next = self.open_count_on(account, endpoint).saturating_sub(1);
        self.set_open_count(account, endpoint, next);
    }

    /// Writes one counter. The (depth account, main feed / order update) pair
    /// has no counter and is ignored: `try_open_on` refuses it before reaching
    /// here, and a release of it has nothing to return.
    fn set_open_count(&mut self, account: DhanAccount, endpoint: DhanEndpointType, value: u8) {
        match (account, endpoint) {
            (DhanAccount::Primary, DhanEndpointType::MainFeed) => self.main_feed = value,
            (DhanAccount::Primary, DhanEndpointType::Depth20) => self.depth_20 = value,
            (DhanAccount::Primary, DhanEndpointType::Depth200) => self.depth_200 = value,
            (DhanAccount::Primary, DhanEndpointType::OrderUpdate) => self.order_update = value,
            (DhanAccount::Depth, DhanEndpointType::Depth20) => self.depth_account_depth_20 = value,
            (DhanAccount::Depth, DhanEndpointType::Depth200) => {
                self.depth_account_depth_200 = value;
            }
            (DhanAccount::Depth, DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate) => {}
        }
    }

    /// Loud, counted refusal. Returns the refusal so call sites stay one-liners.
    fn refuse(&self, refusal: PoolBudgetRefusal) -> PoolBudgetRefusal {
        metrics::counter!(
            POOL_BUDGET_REFUSED_METRIC,
            "account" => refusal.account().as_str(),
            "endpoint" => refusal.endpoint().as_str(),
            "reason" => refusal.reason_str(),
        )
        .increment(1);
        warn!(
            account = refusal.account().as_str(),
            endpoint = refusal.endpoint().as_str(),
            reason = refusal.reason_str(),
            total_open = self.total_open(),
            "Dhan connection pool budget refused a connection open — refusing locally is \
             deliberate: Dhan does not reject an over-limit socket, it silently disconnects \
             the OLDEST one with code 805"
        );
        refusal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::reconnect_ladder::{RECONNECT_JITTER_SLOTS, reconnect_jitter_ms};
    use std::collections::BTreeSet;

    // -- cap constants ------------------------------------------------------

    #[test]
    fn test_max_connections_per_type_sums_to_the_total_ceiling() {
        // THIS is what makes a 17th connection unreachable. If a future edit
        // raises a per-type cap without raising the ceiling (or vice versa),
        // the build fails here rather than at 03:00 on a trading day.
        let sum: u16 = DhanEndpointType::ALL
            .iter()
            .map(|e| u16::from(e.max_connections()))
            .sum();
        assert_eq!(
            sum,
            u16::from(MAX_PRIMARY_ACCOUNT_CONNECTIONS),
            "5 main-feed + 5 depth-20 + 5 depth-200 + 1 order-update must equal 16"
        );
        assert_eq!(
            MAX_PRIMARY_ACCOUNT_CONNECTIONS, 16,
            "operator-authorized primary-account ceiling"
        );
        // 2026-09-26: the depth account adds its own 5 + 5, and nothing else.
        let depth_sum: u16 = DhanEndpointType::ALL
            .iter()
            .map(|e| u16::from(DhanAccount::Depth.max_connections(*e)))
            .sum();
        assert_eq!(depth_sum, u16::from(MAX_DEPTH_ACCOUNT_CONNECTIONS));
        assert_eq!(MAX_DEPTH_ACCOUNT_CONNECTIONS, 10);
        assert_eq!(
            MAX_TOTAL_DHAN_CONNECTIONS, 26,
            "16 primary + 10 depth-account = 26"
        );
    }

    #[test]
    fn test_max_connections_matches_the_authorized_table() {
        assert_eq!(DhanEndpointType::MainFeed.max_connections(), 5);
        assert_eq!(DhanEndpointType::Depth20.max_connections(), 5);
        assert_eq!(DhanEndpointType::Depth200.max_connections(), 5);
        assert_eq!(DhanEndpointType::OrderUpdate.max_connections(), 1);
    }

    #[test]
    fn test_max_connections_agree_with_the_shared_constants_in_common() {
        // These caps are NOT redeclared here — they are derived from
        // `crates/common/src/constants.rs`, which already carried every one of
        // them (and documents the per-endpoint independence: "This limit is
        // INDEPENDENT from depth connection limits"). This test pins the
        // derivation so a `usize -> u8` narrowing can never silently truncate
        // a future larger value into a smaller cap.
        assert_eq!(
            usize::from(MAX_MAIN_FEED_CONNECTIONS),
            MAX_WEBSOCKET_CONNECTIONS
        );
        assert_eq!(
            usize::from(MAX_DEPTH_20_CONNECTIONS),
            MAX_TWENTY_DEPTH_CONNECTIONS
        );
        assert_eq!(
            usize::from(MAX_DEPTH_200_CONNECTIONS),
            MAX_TWO_HUNDRED_DEPTH_CONNECTIONS
        );
        assert_eq!(
            MAIN_FEED_INSTRUMENTS_PER_CONNECTION as usize,
            MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION
        );
        assert_eq!(
            DEPTH_20_INSTRUMENTS_PER_CONNECTION as usize,
            MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION
        );
        assert_eq!(
            DEPTH_200_INSTRUMENTS_PER_CONNECTION as usize,
            MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION
        );
    }

    /// The whole point of this helper is that callers never do the
    /// multiplication themselves. If it ever disagrees with its own two
    /// inputs, a caller sizing a subscription set against it would build a
    /// set `plan_pool` then refuses WHOLESALE — taking the endpoint down
    /// rather than degrading it.
    #[test]
    fn test_subscription_capacity_is_connections_times_instruments() {
        for endpoint in [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
            DhanEndpointType::OrderUpdate,
        ] {
            assert_eq!(
                endpoint.subscription_capacity(),
                endpoint.max_connections() as usize
                    * endpoint.max_instruments_per_connection() as usize,
                "{} capacity must equal its own two inputs",
                endpoint.as_str()
            );
        }
        // The authorized envelopes, spelled out so a silent budget change is
        // visible here and not only in the constants.
        assert_eq!(DhanEndpointType::MainFeed.subscription_capacity(), 25_000);
        assert_eq!(DhanEndpointType::Depth20.subscription_capacity(), 250);
        assert_eq!(DhanEndpointType::Depth200.subscription_capacity(), 5);
        assert_eq!(
            DhanEndpointType::OrderUpdate.subscription_capacity(),
            0,
            "the order-update socket carries no instrument subscriptions"
        );
    }

    #[test]
    fn test_max_instruments_per_connection_matches_dhan_limits() {
        assert_eq!(
            DhanEndpointType::MainFeed.max_instruments_per_connection(),
            5_000
        );
        assert_eq!(
            DhanEndpointType::Depth20.max_instruments_per_connection(),
            50
        );
        assert_eq!(
            DhanEndpointType::Depth200.max_instruments_per_connection(),
            1
        );
        assert_eq!(
            DhanEndpointType::OrderUpdate.max_instruments_per_connection(),
            0
        );
    }

    #[test]
    fn test_max_instruments_per_subscribe_message_is_per_endpoint_not_global() {
        // 15-live-market-feed.md:50 vs 16-full-market-depth.md:58 — the two
        // limits genuinely differ, so a single global constant would be wrong.
        assert_eq!(
            DhanEndpointType::MainFeed.max_instruments_per_subscribe_message(),
            100
        );
        assert_eq!(
            DhanEndpointType::Depth20.max_instruments_per_subscribe_message(),
            50
        );
        assert_ne!(
            DhanEndpointType::MainFeed.max_instruments_per_subscribe_message(),
            DhanEndpointType::Depth20.max_instruments_per_subscribe_message(),
        );
        assert_eq!(
            DhanEndpointType::Depth200.max_instruments_per_subscribe_message(),
            1
        );
        // A subscribe message can never promise more than a connection holds.
        for endpoint in DhanEndpointType::ALL {
            assert!(
                endpoint.max_instruments_per_subscribe_message()
                    <= endpoint.max_instruments_per_connection(),
                "{endpoint}: a single message must not exceed the connection's capacity"
            );
        }
    }

    // -- INDIA FEED ONLY ----------------------------------------------------

    #[test]
    fn test_endpoint_type_has_exactly_four_india_variants_totalling_sixteen() {
        // Operator constraint 2026-08-09: India feed only, exactly four
        // endpoint types, total 16. This makes the scope lock a compile-time
        // property — an added variant fails here, and there is no value a
        // caller can construct to request a fifth endpoint.
        assert_eq!(
            DhanEndpointType::ALL.len(),
            4,
            "exactly four endpoint types"
        );

        // Exhaustive match: adding a variant to the enum stops this compiling
        // until ALL and this arm list are updated together.
        for endpoint in DhanEndpointType::ALL {
            match endpoint {
                DhanEndpointType::MainFeed
                | DhanEndpointType::Depth20
                | DhanEndpointType::Depth200
                | DhanEndpointType::OrderUpdate => {}
            }
        }

        let distinct: BTreeSet<DhanEndpointType> = DhanEndpointType::ALL.into_iter().collect();
        assert_eq!(distinct.len(), 4, "ALL must not repeat a variant");

        let total: u16 = DhanEndpointType::ALL
            .iter()
            .map(|e| u16::from(e.max_connections()))
            .sum();
        assert_eq!(total, 16, "5 + 5 + 5 + 1 = 16");
        assert_eq!(u16::from(MAX_PRIMARY_ACCOUNT_CONNECTIONS), total);
    }

    #[test]
    fn test_pool_budget_can_never_reach_the_us_global_stocks_endpoint() {
        // Operator constraint 2026-08-09: INDIA FEED ONLY. The US global-stocks
        // feed (`wss://global-stocks-api-feed.dhan.co`,
        // 25-global-stocks.md:544) is a FIFTH endpoint type on a different host
        // with a DIFFERENT cap — 6 concurrent connections per clientId
        // (25-global-stocks.md:556) — so folding it in would import the wrong
        // limit as well as the wrong scope.
        //
        // Structural proof, build-failing: this module declares no WebSocket
        // URL at all (hosts live in `crates/common/src/constants.rs` and in
        // `DhanConfig`), and nothing in its production half names the US feed.
        // Needles are assembled with `concat!` so this test's own text does not
        // satisfy the search it performs.
        let src = include_str!("pool_budget.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        // Comment/doc lines are stripped first: this module's own rationale
        // block NAMES the forbidden host and the depth URLs precisely so a
        // future reader understands why they are absent. The guarantee is
        // about CODE, so the scan must be about code too — asserting on prose
        // would only force the explanation to be deleted.
        let production_code: String = src
            .split(test_marker)
            .next()
            .unwrap_or(src)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let production_half = production_code.as_str();

        assert!(
            production_half.contains("DhanEndpointType"),
            "sanity: the production half must still be the real module"
        );
        for needle in [
            concat!("global", "-stocks"),
            concat!("global", "_stocks"),
            concat!("Global", "Stocks"),
        ] {
            assert!(
                !production_half.contains(needle),
                "INDIA FEED ONLY — `{needle}` must not appear in the pool budget's \
                 production code; the US global-stocks feed is a forbidden fifth \
                 endpoint type"
            );
        }

        // And no WebSocket URL is redefined here at all — the depth hosts have
        // exactly one home (constants.rs), where depth-200's ROOT path carries
        // the 2026-04-23 vendor-SDK verification that reversed Dhan's own
        // ticket #5519522. A copy here would silently regress that.
        assert!(
            !production_half.contains(concat!("wss", "://")),
            "this module must declare no WebSocket URL constants — \
             constants.rs is the single WS-URL source"
        );
    }

    #[test]
    fn test_as_str_is_stable_and_unique_per_endpoint() {
        let tags: BTreeSet<&str> = DhanEndpointType::ALL.iter().map(|e| e.as_str()).collect();
        assert_eq!(tags.len(), 4, "metric labels must not collide");
        assert_eq!(DhanEndpointType::MainFeed.as_str(), "main_feed");
        assert_eq!(DhanEndpointType::Depth20.as_str(), "depth_20");
        assert_eq!(DhanEndpointType::Depth200.as_str(), "depth_200");
        assert_eq!(DhanEndpointType::OrderUpdate.as_str(), "order_update");
        assert_eq!(DhanEndpointType::MainFeed.to_string(), "main_feed");
    }

    // -- global index / jitter tiling ---------------------------------------

    #[test]
    fn test_jitter_base_tiles_the_sixteen_slots_exactly() {
        let mut expected_next = 0_u8;
        for endpoint in DhanEndpointType::ALL {
            assert_eq!(
                endpoint.jitter_base(),
                expected_next,
                "{endpoint} must start exactly where the previous pool ended — no \
                 overlap (two connections would share a reconnect delay) and no gap"
            );
            expected_next = expected_next.saturating_add(endpoint.max_connections());
        }
        assert_eq!(
            expected_next, MAX_PRIMARY_ACCOUNT_CONNECTIONS,
            "the four primary pools must tile 0..16 exactly"
        );
    }

    // -- try_open happy path ------------------------------------------------

    #[test]
    fn test_try_open_grants_sequential_pool_and_global_indices() {
        let mut budget = PoolBudget::new();
        for expected in 0..MAX_MAIN_FEED_CONNECTIONS {
            let slot = budget
                .try_open(DhanEndpointType::MainFeed)
                .expect("within cap");
            assert_eq!(slot.endpoint, DhanEndpointType::MainFeed);
            assert_eq!(slot.pool_index, expected);
            assert_eq!(slot.global_index, expected);
        }
        assert_eq!(budget.open_count(DhanEndpointType::MainFeed), 5);
    }

    #[test]
    fn test_try_open_fills_all_sixteen_slots_with_unique_global_indices() {
        let mut budget = PoolBudget::new();
        let mut globals = BTreeSet::new();
        for endpoint in DhanEndpointType::ALL {
            for _ in 0..endpoint.max_connections() {
                let slot = budget.try_open(endpoint).expect("within cap");
                assert!(
                    globals.insert(slot.global_index),
                    "global index {} reused by {endpoint}",
                    slot.global_index
                );
            }
        }
        assert_eq!(globals.len(), 16);
        assert_eq!(budget.total_open(), 16);
        // The whole point of unique global indices: 16 distinct reconnect delays.
        let jitters: BTreeSet<u64> = globals.iter().map(|i| reconnect_jitter_ms(*i)).collect();
        assert_eq!(jitters.len(), 16, "all 16 connections must stagger apart");
    }

    // -- try_open refusals: every per-type boundary --------------------------

    #[test]
    fn test_try_open_refuses_the_sixth_connection_of_every_five_cap_type() {
        for endpoint in [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
        ] {
            let mut budget = PoolBudget::new();
            for _ in 0..5 {
                assert!(budget.try_open(endpoint).is_ok());
            }
            // Total is only 5 here, so this MUST be the per-type arm.
            let refusal = budget.try_open(endpoint).expect_err("6th must be refused");
            assert_eq!(
                refusal,
                PoolBudgetRefusal::EndpointTypeAtCapacity {
                    account: DhanAccount::Primary,
                    endpoint,
                    open: 5,
                    max: 5,
                }
            );
            assert_eq!(refusal.reason_str(), "endpoint_at_capacity");
            assert_eq!(refusal.endpoint(), endpoint);
            assert_eq!(
                budget.open_count(endpoint),
                5,
                "a refusal must not mutate the budget"
            );
        }
    }

    #[test]
    fn test_try_open_refuses_the_second_order_update_connection() {
        let mut budget = PoolBudget::new();
        assert!(budget.try_open(DhanEndpointType::OrderUpdate).is_ok());
        let refusal = budget
            .try_open(DhanEndpointType::OrderUpdate)
            .expect_err("2nd order-update must be refused");
        assert_eq!(
            refusal,
            PoolBudgetRefusal::EndpointTypeAtCapacity {
                account: DhanAccount::Primary,
                endpoint: DhanEndpointType::OrderUpdate,
                open: 1,
                max: 1,
            }
        );
        assert_eq!(budget.total_open(), 1);
    }

    #[test]
    fn test_try_open_allows_exactly_the_cap_for_every_endpoint_type() {
        for endpoint in DhanEndpointType::ALL {
            let mut budget = PoolBudget::new();
            let max = endpoint.max_connections();
            for i in 0..max {
                assert!(
                    budget.try_open(endpoint).is_ok(),
                    "{endpoint} connection {i} must be allowed (cap {max})"
                );
            }
            assert!(
                budget.try_open(endpoint).is_err(),
                "{endpoint} connection {max} (one past the cap) must be refused"
            );
        }
    }

    // -- try_open refusal: the global ceiling -------------------------------

    #[test]
    fn test_try_open_refuses_the_seventeenth_connection_overall() {
        let mut budget = PoolBudget::new();
        for endpoint in DhanEndpointType::ALL {
            for _ in 0..endpoint.max_connections() {
                assert!(budget.try_open(endpoint).is_ok());
            }
        }
        assert_eq!(budget.total_open(), 16);
        // Every type is now full AND the ceiling is reached; the ceiling is
        // checked first, so every further open reports the global arm.
        for endpoint in DhanEndpointType::ALL {
            let refusal = budget.try_open(endpoint).expect_err("17th must be refused");
            assert_eq!(
                refusal,
                PoolBudgetRefusal::TotalAtCapacity {
                    account: DhanAccount::Primary,
                    endpoint,
                    open: 16,
                    max: 16,
                }
            );
            assert_eq!(refusal.reason_str(), "total_at_capacity");
        }
        assert_eq!(budget.total_open(), 16, "refusals must not mutate");
    }

    #[test]
    fn test_try_open_refusal_message_names_the_805_hazard() {
        let mut budget = PoolBudget::new();
        for _ in 0..5 {
            assert!(budget.try_open(DhanEndpointType::MainFeed).is_ok());
        }
        let msg = budget
            .try_open(DhanEndpointType::MainFeed)
            .expect_err("refused")
            .to_string();
        assert!(msg.contains("main_feed"), "{msg}");
        assert!(
            msg.contains("805"),
            "operators must see WHY we refuse: {msg}"
        );
    }

    // -- total_open / open_count -------------------------------------------

    #[test]
    fn test_total_open_and_open_count_track_every_type_independently() {
        let mut budget = PoolBudget::new();
        assert_eq!(budget.total_open(), 0);
        for endpoint in DhanEndpointType::ALL {
            assert_eq!(budget.open_count(endpoint), 0);
        }
        assert!(budget.try_open(DhanEndpointType::MainFeed).is_ok());
        assert!(budget.try_open(DhanEndpointType::Depth200).is_ok());
        assert_eq!(budget.open_count(DhanEndpointType::MainFeed), 1);
        assert_eq!(budget.open_count(DhanEndpointType::Depth20), 0);
        assert_eq!(budget.open_count(DhanEndpointType::Depth200), 1);
        assert_eq!(budget.open_count(DhanEndpointType::OrderUpdate), 0);
        assert_eq!(budget.total_open(), 2);
    }

    // -- release ------------------------------------------------------------

    #[test]
    fn test_release_frees_a_slot_for_reuse() {
        let mut budget = PoolBudget::new();
        for _ in 0..5 {
            assert!(budget.try_open(DhanEndpointType::MainFeed).is_ok());
        }
        assert!(budget.try_open(DhanEndpointType::MainFeed).is_err());
        budget.release(DhanEndpointType::MainFeed);
        assert_eq!(budget.open_count(DhanEndpointType::MainFeed), 4);
        let slot = budget
            .try_open(DhanEndpointType::MainFeed)
            .expect("slot freed");
        assert_eq!(slot.pool_index, 4);
    }

    #[test]
    fn test_release_saturates_at_zero_and_never_underflows() {
        let mut budget = PoolBudget::new();
        for _ in 0..10 {
            budget.release(DhanEndpointType::Depth20);
        }
        assert_eq!(budget.open_count(DhanEndpointType::Depth20), 0);
        assert_eq!(budget.total_open(), 0);
        // An underflow to 255 would have let 255 connections "close" and then
        // silently permitted an over-open; prove the cap still holds.
        for _ in 0..5 {
            assert!(budget.try_open(DhanEndpointType::Depth20).is_ok());
        }
        assert!(budget.try_open(DhanEndpointType::Depth20).is_err());
    }

    #[test]
    fn test_release_only_affects_the_named_endpoint_type() {
        let mut budget = PoolBudget::new();
        assert!(budget.try_open(DhanEndpointType::MainFeed).is_ok());
        assert!(budget.try_open(DhanEndpointType::Depth20).is_ok());
        budget.release(DhanEndpointType::MainFeed);
        assert_eq!(budget.open_count(DhanEndpointType::MainFeed), 0);
        assert_eq!(budget.open_count(DhanEndpointType::Depth20), 1);
    }

    // -- churn --------------------------------------------------------------

    #[test]
    fn test_try_open_and_release_cycle_never_exceeds_the_ceiling() {
        let mut budget = PoolBudget::new();
        for round in 0..50 {
            for endpoint in DhanEndpointType::ALL {
                // Deliberately over-request: the budget must absorb it.
                for _ in 0..(endpoint.max_connections() + 3) {
                    let _ = budget.try_open(endpoint);
                }
            }
            assert_eq!(budget.total_open(), 16, "round {round}");
            assert!(budget.total_open() <= u16::from(MAX_TOTAL_DHAN_CONNECTIONS));
            for endpoint in DhanEndpointType::ALL {
                assert!(budget.open_count(endpoint) <= endpoint.max_connections());
                budget.release(endpoint);
            }
        }
    }

    #[test]
    fn test_reason_str_and_endpoint_accessors_cover_both_refusal_arms() {
        let a = PoolBudgetRefusal::EndpointTypeAtCapacity {
            account: DhanAccount::Primary,
            endpoint: DhanEndpointType::Depth200,
            open: 5,
            max: 5,
        };
        let b = PoolBudgetRefusal::TotalAtCapacity {
            account: DhanAccount::Primary,
            endpoint: DhanEndpointType::Depth20,
            open: 16,
            max: 16,
        };
        assert_eq!(a.reason_str(), "endpoint_at_capacity");
        assert_eq!(b.reason_str(), "total_at_capacity");
        assert_eq!(a.endpoint(), DhanEndpointType::Depth200);
        assert_eq!(b.endpoint(), DhanEndpointType::Depth20);
        assert_ne!(a.reason_str(), b.reason_str());
        let c = PoolBudgetRefusal::EndpointNotPermittedOnAccount {
            account: DhanAccount::Depth,
            endpoint: DhanEndpointType::OrderUpdate,
        };
        assert_eq!(c.reason_str(), "endpoint_not_permitted");
        assert_eq!(c.endpoint(), DhanEndpointType::OrderUpdate);
        assert_eq!(c.account(), DhanAccount::Depth);
        assert_eq!(a.account(), DhanAccount::Primary);
        assert_eq!(b.account(), DhanAccount::Primary);
        let reasons: BTreeSet<&str> = [a, b, c].iter().map(|r| r.reason_str()).collect();
        assert_eq!(reasons.len(), 3, "reason labels must not collide");
    }

    // -- the second, depth-only account (2026-09-26) ------------------------

    #[test]
    fn test_account_as_str_is_stable_and_unique() {
        assert_eq!(DhanAccount::Primary.as_str(), "primary");
        assert_eq!(DhanAccount::Depth.as_str(), "depth");
        assert_eq!(DhanAccount::Depth.to_string(), "depth");
        let tags: BTreeSet<&str> = DhanAccount::ALL.iter().map(|a| a.as_str()).collect();
        assert_eq!(tags.len(), DhanAccount::ALL.len());
    }

    #[test]
    fn test_depth_account_permits_depth_sockets_only() {
        for endpoint in DhanEndpointType::ALL {
            assert!(DhanAccount::Primary.permits(endpoint));
            assert_eq!(
                DhanAccount::Primary.max_connections(endpoint),
                endpoint.max_connections(),
                "the primary account's caps are the pre-2026-09-26 caps, unchanged"
            );
            assert_eq!(
                DhanAccount::Primary.jitter_base(endpoint),
                endpoint.jitter_base(),
                "the primary account's slots are the pre-2026-09-26 slots, unchanged"
            );
        }
        assert!(DhanAccount::Depth.permits(DhanEndpointType::Depth20));
        assert!(DhanAccount::Depth.permits(DhanEndpointType::Depth200));
        assert!(!DhanAccount::Depth.permits(DhanEndpointType::MainFeed));
        assert!(!DhanAccount::Depth.permits(DhanEndpointType::OrderUpdate));
        assert_eq!(
            DhanAccount::Depth.max_connections(DhanEndpointType::MainFeed),
            0
        );
        assert_eq!(
            DhanAccount::Depth.max_connections(DhanEndpointType::OrderUpdate),
            0
        );
    }

    #[test]
    fn test_depth_account_refuses_main_feed_and_order_update_without_mutating() {
        let mut budget = PoolBudget::new();
        for endpoint in [DhanEndpointType::MainFeed, DhanEndpointType::OrderUpdate] {
            let refusal = budget
                .try_open_on(DhanAccount::Depth, endpoint)
                .expect_err("the depth account never opens this endpoint");
            assert_eq!(
                refusal,
                PoolBudgetRefusal::EndpointNotPermittedOnAccount {
                    account: DhanAccount::Depth,
                    endpoint,
                }
            );
            assert!(refusal.to_string().contains("depth-20 and depth-200"));
        }
        assert_eq!(budget, PoolBudget::new(), "a refusal must not mutate");
        // Releasing a pair with no counter is a harmless no-op.
        budget.release_on(DhanAccount::Depth, DhanEndpointType::MainFeed);
        assert_eq!(budget, PoolBudget::new());
    }

    #[test]
    fn test_both_accounts_tile_all_twenty_six_slots_exactly_once() {
        let mut budget = PoolBudget::new();
        let mut globals = BTreeSet::new();
        for account in DhanAccount::ALL {
            for endpoint in DhanEndpointType::ALL {
                for expected_pool_index in 0..account.max_connections(endpoint) {
                    let slot = budget.try_open_on(account, endpoint).expect("within cap");
                    assert_eq!(slot.account, account);
                    assert_eq!(slot.endpoint, endpoint);
                    assert_eq!(slot.pool_index, expected_pool_index);
                    assert!(
                        globals.insert(slot.global_index),
                        "slot {} granted twice",
                        slot.global_index
                    );
                    assert_eq!(
                        slot_owner(slot.global_index),
                        Some((account, endpoint)),
                        "slot_owner must name the pool that granted slot {}",
                        slot.global_index
                    );
                }
            }
        }
        let expected: BTreeSet<u8> = (0..MAX_TOTAL_DHAN_CONNECTIONS).collect();
        assert_eq!(
            globals, expected,
            "the two accounts must tile 0..26 exactly"
        );
        assert_eq!(budget.total_open(), u16::from(MAX_TOTAL_DHAN_CONNECTIONS));
        assert_eq!(budget.total_open_on(DhanAccount::Primary), 16);
        assert_eq!(budget.total_open_on(DhanAccount::Depth), 10);
        // Every slot gets its own reconnect stagger.
        assert_eq!(
            MAX_TOTAL_DHAN_CONNECTIONS, RECONNECT_JITTER_SLOTS,
            "the slot space must match the jitter slot count or a connection wraps \
             onto another's stagger"
        );
        let jitters: BTreeSet<u64> = globals.iter().map(|i| reconnect_jitter_ms(*i)).collect();
        assert_eq!(jitters.len(), usize::from(MAX_TOTAL_DHAN_CONNECTIONS));
        // The depth account's slots are exactly 16..26.
        for slot in 16..26 {
            assert_eq!(slot_owner(slot).map(|(a, _)| a), Some(DhanAccount::Depth));
        }
        assert_eq!(slot_owner(MAX_TOTAL_DHAN_CONNECTIONS), None);
        assert_eq!(slot_owner(u8::MAX), None);
    }

    #[test]
    fn test_the_two_accounts_are_counted_independently() {
        let mut budget = PoolBudget::new();
        for _ in 0..5 {
            assert!(budget.try_open(DhanEndpointType::Depth200).is_ok());
        }
        // The primary depth-200 pool is full; the depth account's is untouched.
        assert!(budget.try_open(DhanEndpointType::Depth200).is_err());
        for i in 0..5 {
            let slot = budget
                .try_open_on(DhanAccount::Depth, DhanEndpointType::Depth200)
                .expect("the depth account has its own five");
            assert_eq!(slot.global_index, 21 + i);
        }
        let refusal = budget
            .try_open_on(DhanAccount::Depth, DhanEndpointType::Depth200)
            .expect_err("sixth depth-account depth-200");
        assert_eq!(
            refusal,
            PoolBudgetRefusal::EndpointTypeAtCapacity {
                account: DhanAccount::Depth,
                endpoint: DhanEndpointType::Depth200,
                open: 5,
                max: 5,
            }
        );
        assert_eq!(budget.open_count(DhanEndpointType::Depth200), 5);
        assert_eq!(
            budget.open_count_on(DhanAccount::Depth, DhanEndpointType::Depth200),
            5
        );
        // Releasing on one account never frees the other's slot.
        budget.release_on(DhanAccount::Depth, DhanEndpointType::Depth200);
        assert_eq!(budget.open_count(DhanEndpointType::Depth200), 5);
        assert_eq!(
            budget.open_count_on(DhanAccount::Depth, DhanEndpointType::Depth200),
            4
        );
        budget.release(DhanEndpointType::Depth200);
        assert_eq!(budget.open_count(DhanEndpointType::Depth200), 4);
        assert_eq!(budget.total_open(), 8);
    }

    #[test]
    fn test_depth_account_ceiling_is_ten_and_refuses_the_eleventh() {
        let mut budget = PoolBudget::new();
        for endpoint in [DhanEndpointType::Depth20, DhanEndpointType::Depth200] {
            for _ in 0..5 {
                assert!(budget.try_open_on(DhanAccount::Depth, endpoint).is_ok());
            }
        }
        for endpoint in [DhanEndpointType::Depth20, DhanEndpointType::Depth200] {
            let refusal = budget
                .try_open_on(DhanAccount::Depth, endpoint)
                .expect_err("eleventh depth-account socket");
            assert_eq!(
                refusal,
                PoolBudgetRefusal::TotalAtCapacity {
                    account: DhanAccount::Depth,
                    endpoint,
                    open: 10,
                    max: 10,
                }
            );
            assert!(refusal.to_string().contains("depth account"));
        }
        // The primary account is untouched by a full depth account.
        assert!(budget.try_open(DhanEndpointType::MainFeed).is_ok());
    }

    #[test]
    fn test_connection_slot_label_covers_every_slot_of_both_accounts() {
        for slot in 0..MAX_TOTAL_DHAN_CONNECTIONS {
            assert_eq!(connection_slot_label(slot), slot.to_string());
        }
        assert_eq!(connection_slot_label(MAX_TOTAL_DHAN_CONNECTIONS), "unknown");
    }

    /// TVW4 (2026-09-02): the WAL endpoint byte is the ONLY thing that lets a
    /// replay route a depth frame to the depth parser. Two endpoint types
    /// sharing a byte would send one of them to the wrong parser on every
    /// replay, silently; a byte that does not survive `from_u8` would degrade
    /// to the main feed the same way.
    #[test]
    fn every_endpoint_type_maps_to_a_distinct_wal_endpoint() {
        use tickvault_storage::ws_frame_spill::WalEndpoint;
        let mut seen = std::collections::BTreeSet::new();
        for endpoint in DhanEndpointType::ALL {
            let wal = endpoint.wal_endpoint();
            assert!(
                seen.insert(wal.as_u8()),
                "{endpoint:?} shares WAL endpoint byte {} with another type",
                wal.as_u8()
            );
            assert_eq!(
                WalEndpoint::from_u8(wal.as_u8()),
                wal,
                "{endpoint:?}'s byte must survive the disk round-trip"
            );
        }
        assert_eq!(seen.len(), DhanEndpointType::ALL.len());
        assert_eq!(
            DhanEndpointType::MainFeed.wal_endpoint(),
            WalEndpoint::MainFeed
        );
        assert_eq!(
            DhanEndpointType::Depth20.wal_endpoint(),
            WalEndpoint::Depth20
        );
        assert_eq!(
            DhanEndpointType::Depth200.wal_endpoint(),
            WalEndpoint::Depth200
        );
        assert_eq!(
            DhanEndpointType::OrderUpdate.wal_endpoint(),
            WalEndpoint::OrderUpdate
        );
    }
}
