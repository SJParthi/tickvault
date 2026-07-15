//! Order Management System (OMS) — order lifecycle, rate limiting, and reconciliation.
//!
//! # Modules
//! - `types` — ManagedOrder, request/response structs, error types
//! - `state_machine` — Order lifecycle transition validation
//! - `api_client` — Dhan REST API client for order operations
//! - `rate_limiter` — SEBI-mandated GCRA rate limiter
//! - `circuit_breaker` — Dhan API circuit breaker
//! - `idempotency` — UUID v4 correlation ID tracking
//! - `margin_gate` — 🔷 DHAN pre-trade margin gate (exits never gated)
//! - `reconciliation` — REST-based state sync
//! - `exit_rules` — pure exit-order rules (slicing math, OCO/CNC-MTF
//!   validation, MPP verdict classification, verify backoff ladder)
//! - `engine` — Orchestrator composing all sub-components
//!
//! # Architecture
//! ```text
//! Strategy Signal
//!       ↓
//! Risk Engine (pre-trade check)
//!       ↓
//! OMS Engine
//!   ├── Rate Limiter (SEBI 10 orders/sec)
//!   ├── Circuit Breaker (Dhan API health)
//!   ├── Idempotency (UUID correlation)
//!   ├── API Client (REST calls)
//!   └── State Machine (lifecycle tracking)
//!       ↓
//! WebSocket Order Updates → State Transitions
//!       ↓
//! Reconciliation (periodic REST sync)
//! ```

pub mod api_client;
pub mod circuit_breaker;
pub mod dh904_backoff;
pub mod engine;
pub mod exit_rules;
pub mod idempotency;
pub mod margin_gate;
pub mod rate_limiter;
pub mod reconciliation;
pub mod state_machine;
pub mod types;

/// Groww order-side lane — GATED behind the non-default `groww_orders` cargo
/// feature (§39.2 Gate 2). Absent from a default build.
#[cfg(feature = "groww_orders")]
pub mod groww;

// Re-export key types for ergonomic use.
pub use api_client::OrderApiClient;
pub use engine::{OrderManagementSystem, TokenProvider};
pub use exit_rules::ExitCommand;
pub use margin_gate::{MarginGate, MarginSnapshot, MarginVerdict};
pub use rate_limiter::OrderRateLimiter;
pub use types::{
    ExecutionVerdict, ManagedOrder, ManagedSuperOrder, ModifyOrderRequest, ModifySuperOrderLeg,
    OcoSecondLeg, OmsError, OrderIntent, PlaceForeverOcoRequest, PlaceOrderRequest,
    PlaceSuperOrderRequest, ReconciliationReport, SlicingResponse, SuperOrderPlacement,
};
