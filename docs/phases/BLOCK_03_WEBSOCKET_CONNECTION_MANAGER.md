# BLOCK 03 — WEBSOCKET CONNECTION MANAGER

## Status: ✅ COMPLETE & HARDENED
## Last Verified: 2026-02-26 (346 tests pass, zero clippy warnings)
## Depends On: Block 02 (Authentication & Token Management)
## Blocks: Block 04 (Binary Protocol Parser)

---

## OBJECTIVE

Implement the Dhan WebSocket V2 connection lifecycle with multi-connection pooling, instrument distribution, keep-alive management, and reconnection with exponential backoff. This is the transport layer — it establishes authenticated WebSocket connections, subscribes to instruments, maintains keep-alive pings, reads raw binary frames, and forwards them downstream for parsing.

---

## BOOT CHAIN POSITION

```
Config → Auth → ★ WEBSOCKET (Block 03) ★ → Parse (Block 04) → Route → Indicators
                │
                ├─ Read token from ArcSwap (O(1) atomic)
                ├─ Connect wss://api-feed.dhan.co/v2 with auth headers
                ├─ Send batched JSON subscriptions (≤100/msg)
                ├─ Ping every 10s (Dhan spec), detect pong failures
                ├─ Read binary frames → forward via mpsc channel
                ├─ Handle disconnect codes (803/804 → token refresh)
                └─ Reconnect with exponential backoff (500ms → 30s)
```

---

## DHAN WEBSOCKET V2 — CONNECTION LIMITS

| Parameter | Value | Source |
|-----------|-------|--------|
| Endpoint | `wss://api-feed.dhan.co/v2` | config.dhan.websocket_url |
| Protocol | Binary frames, Little Endian | Dhan spec |
| Auth header | `Authorization: <access_token>` | TokenHandle (ArcSwap) |
| Client ID header | `Client-Id: <dhan_client_id>` | SSM credentials |
| Max connections | 5 per account | DISCONNECT_EXCEEDED_MAX_CONNECTIONS (801) |
| Max instruments/connection | 5,000 | DISCONNECT_EXCEEDED_MAX_INSTRUMENTS (802) |
| Total capacity | 25,000 instruments | DISCONNECT_SUBSCRIPTION_LIMIT (808) |
| Max instruments/message | 100 | SUBSCRIPTION_BATCH_SIZE constant |
| Ping interval | 10 seconds | config.websocket.ping_interval_secs |
| Server timeout | 40 seconds without ping | DISCONNECT_PING_TIMEOUT (806) |
| Pong timeout | 10 seconds | config.websocket.pong_timeout_secs |
| Max consecutive pong failures | 2 | config.websocket.max_consecutive_pong_failures |

---

## DISCONNECT CODES — RECOVERY ACTIONS

| Code | Enum Variant | Reconnectable? | Token Refresh? | Action |
|------|-------------|----------------|---------------|--------|
| 801 | ExceededMaxConnections | ❌ | No | Reduce connections — config bug |
| 802 | ExceededMaxInstruments | ❌ | No | Reduce instruments — config bug |
| 803 | AuthenticationFailed | ✅ | **YES** | Refresh token, then reconnect |
| 804 | TokenExpired | ✅ | **YES** | Refresh token, then reconnect |
| 805 | ServerMaintenance | ✅ | No | Backoff + reconnect |
| 806 | PingTimeout | ✅ | No | Bug in ping logic — fix + reconnect |
| 807 | InvalidSubscription | ❌ | No | Fix request format — code bug |
| 808 | SubscriptionLimitExceeded | ❌ | No | Reduce total instruments |
| 810 | ForceKilled | ✅ | No | Wait + retry later |
| 814 | InvalidClientId | ❌ | No | Check SSM credentials |
| Unknown(n) | Unknown | ✅ (assume transient) | No | Log + reconnect |

---

## SUBSCRIPTION PROTOCOL

### Request Format (JSON over WebSocket Text frame)

```json
{
  "RequestCode": 21,
  "InstrumentCount": 3,
  "InstrumentList": [
    { "ExchangeSegment": "IDX_I", "SecurityId": "13" },
    { "ExchangeSegment": "NSE_EQ", "SecurityId": "2885" },
    { "ExchangeSegment": "NSE_FNO", "SecurityId": "52432" }
  ]
}
```

**RequestCode values:**
- 15 = Ticker (LTP + OHLC, 25 bytes)
- 17 = Quote (Ticker + bid/ask, 51 bytes)
- 21 = Full (Quote + OI + 5-level depth, 162 bytes)
- 12 = Unsubscribe

**Rules:**
- SecurityId is always a **STRING** in JSON (not numeric)
- Max 100 instruments per message (Dhan limit)
- For >100 instruments, builder creates multiple batched messages
- batch_size clamped to [1, 100] regardless of caller input

---

## FILES CREATED

| File | Purpose | Lines | Tests |
|------|---------|-------|-------|
| `crates/core/src/websocket/mod.rs` | Module re-exports | 24 | — |
| `crates/core/src/websocket/types.rs` | ConnectionState, DisconnectCode, WebSocketError, InstrumentSubscription, ConnectionHealth | ~270 | 30 |
| `crates/core/src/websocket/subscription_builder.rs` | JSON batch builder (subscribe + unsubscribe) | ~90 | 22 |
| `crates/core/src/websocket/connection.rs` | Single WebSocket lifecycle (connect, subscribe, ping, read, reconnect) | ~420 | 12 |
| `crates/core/src/websocket/connection_pool.rs` | Multi-connection pool with round-robin instrument distribution | ~350 | 18 |

## FILES MODIFIED

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Added `futures-util = "0.3.31"` |
| `crates/core/Cargo.toml` | Added `futures-util = { workspace = true }` |
| `crates/common/src/constants.rs` | Fixed PING_INTERVAL_SECS (30→10), added 30+ WebSocket constants |
| `crates/common/src/config.rs` | Added WebSocketConfig (7 fields), Clone on DhanConfig, 6 validation tests |
| `config/base.toml` | Added `[websocket]` section |
| `crates/core/src/auth/token_manager.rs` | Exported `TokenHandle` type alias |
| `crates/core/src/auth/types.rs` | Added `access_token()` accessor on TokenState |
| `crates/core/src/lib.rs` | Added `pub mod websocket` |

---

## PUBLIC API

### types.rs

```rust
type ConnectionId = u8;  // 0–4

enum ConnectionState { Disconnected, Connecting, Connected, Reconnecting }
enum DisconnectCode { ExceededMaxConnections, ..., Unknown(u16) }
enum WebSocketError { ConnectionFailed, NoTokenAvailable, DhanDisconnect, ... }

struct InstrumentSubscription { exchange_segment: String, security_id: String }
struct SubscriptionRequest { request_code: u8, instrument_count: usize, instrument_list: Vec }
struct ConnectionHealth { connection_id, state, subscribed_count, consecutive_pong_failures, total_reconnections }

DisconnectCode::from_u16(code) -> Self
DisconnectCode::as_u16(&self) -> u16
DisconnectCode::is_reconnectable(&self) -> bool
DisconnectCode::requires_token_refresh(&self) -> bool
InstrumentSubscription::new(segment: ExchangeSegment, security_id: u32) -> Self
```

### subscription_builder.rs

```rust
build_subscription_messages(instruments, feed_mode, batch_size) -> Vec<String>
build_unsubscription_messages(instruments, batch_size) -> Vec<String>
```

### connection.rs

```rust
WebSocketConnection::new(id, token, client_id, dhan_config, ws_config, instruments, feed_mode, sender) -> Self
WebSocketConnection::run(&self) -> Result<(), WebSocketError>  // async lifecycle
WebSocketConnection::health(&self) -> ConnectionHealth
WebSocketConnection::connection_id(&self) -> ConnectionId
```

### connection_pool.rs

```rust
WebSocketConnectionPool::new(token, client_id, dhan_config, ws_config, instruments, feed_mode) -> Result<Self>
WebSocketConnectionPool::spawn_all(&self) -> Vec<JoinHandle<Result<()>>>
WebSocketConnectionPool::take_frame_receiver(&mut self) -> mpsc::Receiver<Vec<u8>>
WebSocketConnectionPool::health(&self) -> Vec<ConnectionHealth>
WebSocketConnectionPool::connection_count(&self) -> usize
WebSocketConnectionPool::total_instruments(&self) -> usize
```

---

## CONNECTION LIFECYCLE

```
1. Pool::new() → calculate min connections, distribute instruments round-robin
2. Pool::spawn_all() → each connection runs as independent tokio task
3. Connection::run() → main loop:
   a. Read token from ArcSwap (O(1))
   b. Build HTTP request with Authorization + Client-Id headers
   c. connect_async() with timeout
   d. Split stream → send batched subscriptions (≤100/msg)
   e. Reunite stream → split into read + write halves
   f. Spawn ping task (10s interval via tokio::time::interval)
   g. Read loop: Binary → forward to mpsc, Pong → reset counter, Close → handle
   h. On disconnect: check is_reconnectable()
      - Non-reconnectable → return error, stop connection
      - Reconnectable → exponential backoff (500ms * 2^attempt, max 30s)
   i. If max attempts exhausted → return ReconnectionExhausted
4. Pool::take_frame_receiver() → downstream gets mpsc::Receiver<Vec<u8>>
```

---

## CONFIGURATION (config/base.toml)

```toml
[websocket]
ping_interval_secs = 10
pong_timeout_secs = 10
max_consecutive_pong_failures = 2
reconnect_initial_delay_ms = 500
reconnect_max_delay_ms = 30000
reconnect_max_attempts = 10
subscription_batch_size = 100
```

**Validation rules:**
- `ping_interval_secs` must be > 0
- `subscription_batch_size` must be in [1, 100]
- `reconnect_max_attempts` must be > 0

---

## DEPENDENCY ADDED

| Crate | Version | Purpose | In Bible? |
|-------|---------|---------|-----------|
| futures-util | 0.3.31 | `StreamExt`/`SinkExt` for WebSocket stream splitting | No — required for tokio-tungstenite stream ops |

---

## CONSTANTS ADDED (constants.rs)

```rust
// Keep-alive
PING_INTERVAL_SECS = 10 (fixed from 30)
SERVER_PING_TIMEOUT_SECS = 40
MAX_CONSECUTIVE_PONG_FAILURES = 2
SUBSCRIPTION_BATCH_SIZE = 100

// Disconnect codes (801–814)
DISCONNECT_EXCEEDED_MAX_CONNECTIONS = 801
DISCONNECT_EXCEEDED_MAX_INSTRUMENTS = 802
DISCONNECT_AUTH_FAILED = 803
DISCONNECT_TOKEN_EXPIRED = 804
DISCONNECT_SERVER_MAINTENANCE = 805
DISCONNECT_PING_TIMEOUT = 806
DISCONNECT_INVALID_SUBSCRIPTION = 807
DISCONNECT_SUBSCRIPTION_LIMIT = 808
DISCONNECT_FORCE_KILLED = 810
DISCONNECT_INVALID_CLIENT_ID = 814

// Response codes
RESPONSE_CODE_INDEX_TICKER = 1
RESPONSE_CODE_TICKER = 2
RESPONSE_CODE_QUOTE = 4
RESPONSE_CODE_OI_DATA = 5
RESPONSE_CODE_PREV_CLOSE = 6
RESPONSE_CODE_MARKET_STATUS = 7
RESPONSE_CODE_FULL_PACKET = 8
RESPONSE_CODE_DISCONNECT = 50

// Feed request codes
FEED_REQUEST_TICKER = 15
FEED_REQUEST_QUOTE = 17
FEED_REQUEST_FULL = 21
FEED_REQUEST_UNSUBSCRIBE = 12

// Binary protocol
BINARY_HEADER_SIZE = 8
OI_PACKET_SIZE = 17
MARKET_STATUS_OPEN = 2, CLOSED = 0, PRE_OPEN = 1, POST_CLOSE = 3
```

---

## TEST COVERAGE (82 tests across Block 03)

| Module | Tests | Key Scenarios |
|--------|-------|---------------|
| types.rs | 30 | All disconnect codes (from_u16, roundtrip, reconnectable, token refresh), all Display variants, Clone/Copy/Send/Sync, serialization roundtrips, ConnectionHealth clone |
| subscription_builder.rs | 22 | Empty, single, exact boundary (100), split (101), multiple batches, batch_size clamping (0→1, 500→100), all feed modes, all segments, u32::MAX, mixed segments, unsubscribe, valid JSON parse |
| connection.rs | 12 | Initial state, connection_id, instrument count, all feed modes, max id (4), state transitions via set_state, pong failure tracking, reconnection count, backoff exhaustion, backoff under limit, run without token, ReconnectionExhausted error type |
| connection_pool.rs | 18 | Empty (1 conn), single, under/at/over 5000, all boundaries (4999/10000/15000/20000/25000), capacity exceeded (25001), round-robin distribution, health reporting, take_frame_receiver once, Debug impl, config override, total_instruments integrity (11 counts) |
| config.rs | 6 | Zero ping, zero batch, batch >100, zero reconnect_attempts, boundary (1, 100) |

---

## WHAT BLOCK 04 NEEDS FROM THIS

Block 04 (Binary Protocol Parser) receives raw `Vec<u8>` frames from `WebSocketConnectionPool::take_frame_receiver()`. Each frame is a complete Dhan binary packet starting with the 8-byte header. Block 04 will:

1. Read the 8-byte header (response_code, length, segment, security_id)
2. Dispatch to the correct parser based on response_code
3. Parse into typed structs (TickerPacket, QuotePacket, FullPacket, etc.)
4. Forward parsed data to the routing layer via SPSC ring buffer (rtrb)

---

## FUTURE CHANGES — WHEN TO UPDATE THIS DOC

- If Dhan changes WebSocket V2 protocol (new disconnect codes, packet formats)
- If connection limits change (>5 connections, >5000/conn, >25000 total)
- If keep-alive intervals change from Dhan side
- If new subscription modes are added beyond Ticker/Quote/Full
- If reconnection strategy changes (circuit breaker, different backoff)
- If token refresh integration is modified in connection.rs
