//! Order Management System, risk controls, and order execution.
//!
//! # Key Modules (to be built)
//! - `order_management` — statig state machine for order lifecycle
//! - `risk_manager` — Position sizing, drawdown limits, P&L tracking
//! - `order_executor` — Dhan REST API order submission with governor rate limiting
//!
//! # Boot Sequence Position
//! State -> **OMS -> Risk -> Execute** -> Persist
