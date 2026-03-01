// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![warn(missing_docs)]

//! Order Management System, risk controls, and order execution.
//!
//! # Key Modules (to be built)
//! - `order_management` — statig state machine for order lifecycle
//! - `risk_manager` — Position sizing, drawdown limits, P&L tracking
//! - `order_executor` — Dhan REST API order submission with governor rate limiting
//!
//! # Boot Sequence Position
//! State -> **OMS -> Risk -> Execute** -> Persist
