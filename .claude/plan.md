# OMS Implementation Plan — Block 10

## What Exists Today

| Component | Location | Status |
|-----------|----------|--------|
| Order types (enums, OrderUpdate struct) | `crates/common/src/order_types.rs` | Done |
| Order update parser + login builder | `crates/core/src/parser/order_update.rs` | Done |
| Order update WebSocket connection | `crates/core/src/websocket/order_update_connection.rs` | Done |
| Risk engine (daily loss, position limits, P&L) | `crates/trading/src/risk/` | Done |
| Config (rest_api_base_url, max_orders_per_second, order_cutoff_time) | `crates/common/src/config.rs` | Done |
| Workspace deps (statig, governor, failsafe, uuid) | `Cargo.toml` | Pinned |

## What Needs Building (6 items from Phase 1 checklist)

All code goes in `crates/trading/src/oms/` — new module inside the existing `trading` crate.

---

### Step 1: OMS State Machine (`oms/state_machine.rs`)

**What:** `statig` state machine for the full order lifecycle.

**States:** `Transit → Pending → Confirmed → Traded / Cancelled / Rejected / Expired`

**Valid transitions** (from phase doc §9):
```
Transit  → Pending     (reached exchange)
Transit  → Rejected    (exchange rejected immediately)
Pending  → Confirmed   (exchange accepted)
Confirmed → Traded     (fully filled)
Confirmed → Cancelled  (user cancelled)
Confirmed → Expired    (end of validity)
Pending  → Traded      (immediate fill, skips Confirmed)
Pending  → Cancelled   (cancelled before confirmation)
```

**Invalid transitions** → `tracing::error!` + Telegram alert + reconciliation flag.

**Internal order record** (`ManagedOrder`):
```rust
struct ManagedOrder {
    order_id: String,          // Dhan order ID (from place response)
    correlation_id: String,    // Our UUID (idempotency key)
    security_id: u32,
    transaction_type: TransactionType,
    order_type: OrderType,
    product_type: ProductType,
    quantity: i64,
    price: f64,
    trigger_price: f64,
    status: OrderStatus,
    traded_qty: i64,
    avg_traded_price: f64,
    created_at: i64,           // epoch micros
    updated_at: i64,           // epoch micros
    needs_reconciliation: bool,
}
```

**Storage:** `HashMap<String, ManagedOrder>` keyed by `order_id`. Secondary index `HashMap<String, String>` mapping `correlation_id → order_id`.

---

### Step 2: Order API Client (`oms/api_client.rs`)

**What:** Typed reqwest client wrapping the 5 Dhan REST endpoints.

**Endpoints** (from phase doc §9):
| Method | Path | Purpose |
|--------|------|---------|
| POST   | `/v2/orders` | Place order |
| PUT    | `/v2/orders/{orderId}` | Modify order |
| DELETE | `/v2/orders/{orderId}` | Cancel order |
| GET    | `/v2/orders/{orderId}` | Get order by ID |
| GET    | `/v2/orders` | Get all orders (today) |
| GET    | `/v2/positions` | Get positions |

**Request/response types:** Serde structs matching Dhan's JSON format (camelCase).

**Token:** Read from `TokenHandle` (arc-swap) on each call.

**Dependencies:** `reqwest` (already in workspace), `secrecy` for token handling.

---

### Step 3: Rate Limiter (`oms/rate_limiter.rs`)

**What:** SEBI-mandated 10 orders/sec using `governor` crate (GCRA algorithm).

- Wraps `governor::RateLimiter` with quota from `config.trading.max_orders_per_second`
- On rate limit hit: return error immediately (DO NOT retry — SEBI violation risk)
- On broker HTTP 429 (DH-300): back off, alert, do NOT retry
- Cold path — order submission is infrequent

---

### Step 4: Circuit Breaker (`oms/circuit_breaker.rs`)

**What:** `failsafe` circuit breaker around the Dhan REST API.

- States: Closed (normal) → Open (failing) → Half-Open (probe)
- Threshold: 3 consecutive failures → open for 30s → half-open
- On open: reject order submission immediately with error
- Config-driven thresholds (constants in `common/constants.rs`)

---

### Step 5: Idempotency (`oms/idempotency.rs`)

**What:** UUID v4 correlation IDs for order idempotency.

- Generate `uuid::Uuid::new_v4()` for each place order request
- Track `correlation_id → order_id` mapping in the state machine's secondary index
- Dhan echoes `correlationId` back in response and WebSocket updates — use it to match
- No Valkey needed yet (single instance) — in-memory HashMap sufficient for Phase 1

---

### Step 6: Position Tracking & Reconciliation (`oms/reconciliation.rs`)

**What:** Reconcile OMS state with Dhan REST API on reconnect or periodic check.

- On order update WebSocket reconnect: call `GET /v2/orders` to get all today's orders
- Compare each order's status with our `ManagedOrder` state
- Mismatches → `tracing::error!` + Telegram alert + update local state
- On startup: fetch all orders + positions to seed OMS state
- Integrate with `RiskEngine::record_fill()` for position updates

---

### Step 7: OMS Orchestrator (`oms/mod.rs` + `oms/engine.rs`)

**What:** Top-level `OrderManagementSystem` struct that composes all sub-components.

```rust
pub struct OrderManagementSystem {
    state: HashMap<String, ManagedOrder>,        // order_id → order
    correlation_index: HashMap<String, String>,  // correlation_id → order_id
    api_client: OrderApiClient,
    rate_limiter: OrderRateLimiter,
    circuit_breaker: OrderCircuitBreaker,
}
```

**Public API:**
```rust
impl OrderManagementSystem {
    pub async fn place_order(&mut self, request: PlaceOrderRequest) -> Result<String>;
    pub async fn modify_order(&mut self, order_id: &str, request: ModifyOrderRequest) -> Result<()>;
    pub async fn cancel_order(&mut self, order_id: &str) -> Result<()>;
    pub fn handle_order_update(&mut self, update: &OrderUpdate) -> Result<()>;
    pub async fn reconcile(&mut self) -> Result<ReconciliationReport>;
    pub fn order(&self, order_id: &str) -> Option<&ManagedOrder>;
    pub fn active_orders(&self) -> Vec<&ManagedOrder>;
}
```

**Flow for `place_order`:**
1. Risk engine pre-check → reject if breached
2. Rate limiter check → reject if throttled
3. Circuit breaker check → reject if open
4. Generate correlation ID (UUID v4)
5. Call Dhan REST API
6. Create `ManagedOrder` in `Transit` state
7. Return order_id

**Flow for `handle_order_update`** (from WebSocket):
1. Look up `ManagedOrder` by `order_id` (or `correlation_id`)
2. Validate state transition
3. Update status, traded qty, avg price
4. If filled → `RiskEngine::record_fill()`
5. If invalid transition → error log + reconciliation flag

---

### Step 8: Wire Into Boot Sequence + Order Update WebSocket

- Add `oms` module to `crates/trading/src/lib.rs`
- Wire OMS as subscriber to the existing `broadcast::Sender<OrderUpdate>` channel
- Block 11's pending items: OMS state transition from updates + reconciliation on reconnect

---

## File Plan

```
crates/trading/src/oms/
├── mod.rs                 // Module exports
├── types.rs               // ManagedOrder, PlaceOrderRequest, ModifyOrderRequest, errors
├── state_machine.rs       // statig FSM for order lifecycle
├── api_client.rs          // reqwest wrapper for Dhan order REST API
├── rate_limiter.rs        // governor GCRA rate limiter
├── circuit_breaker.rs     // failsafe circuit breaker
├── idempotency.rs         // UUID correlation ID generation + tracking
├── reconciliation.rs      // REST reconciliation on reconnect/startup
├── engine.rs              // OrderManagementSystem orchestrator
└── tests.rs               // Integration tests
```

**Dependency additions to `crates/trading/Cargo.toml`:**
- `reqwest = { workspace = true }` (for REST API calls)
- `secrecy = { workspace = true }` (for token handling)
- `failsafe = { workspace = true }` (for circuit breaker)
- `serde_json = { workspace = true }` (move from dev-deps to deps)

All versions already pinned in workspace root.

---

## Execution Order

1. `types.rs` — ManagedOrder, request/response structs, error types
2. `state_machine.rs` — statig FSM with valid transitions
3. `api_client.rs` — Dhan REST wrappers
4. `rate_limiter.rs` — governor GCRA
5. `circuit_breaker.rs` — failsafe wrapper
6. `idempotency.rs` — UUID generation + correlation index
7. `reconciliation.rs` — REST-based state sync
8. `engine.rs` — orchestrator composing all above
9. `mod.rs` — module tree + re-exports
10. Wire into `lib.rs`
11. Tests for each module
12. `cargo fmt && cargo clippy && cargo test`

---

## Principles Check

| Principle | How |
|-----------|-----|
| Zero allocation on hot path | OMS is cold path (~1-100 orders/day). Allocations acceptable. |
| O(1) or fail at compile time | HashMap lookups for order state. Rate limiter is O(1) GCRA. |
| Every version pinned | All deps already in workspace with exact versions. |
