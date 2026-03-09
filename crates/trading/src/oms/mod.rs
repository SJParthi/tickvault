//! Order Management System (OMS) — order lifecycle, rate limiting, and reconciliation.
//!
//! # Modules
//! - `types` — ManagedOrder, request/response structs, error types
//! - `state_machine` — Order lifecycle transition validation
//! - `api_client` — Dhan REST API client for order operations
//! - `rate_limiter` — SEBI-mandated GCRA rate limiter
//! - `circuit_breaker` — Dhan API circuit breaker
//! - `idempotency` — UUID v4 correlation ID tracking
//! - `reconciliation` — REST-based state sync
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
pub mod engine;
pub mod idempotency;
pub mod rate_limiter;
pub mod reconciliation;
pub mod state_machine;
pub mod types;

// Re-export key types for ergonomic use.
pub use engine::{OrderManagementSystem, TokenProvider};
pub use types::{ManagedOrder, ModifyOrderRequest, OmsError, PlaceOrderRequest};
