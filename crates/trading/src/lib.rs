// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

//! Order Management System, risk controls, and order execution.
//!
//! # Key Modules (to be built)
//! - `order_management` — statig state machine for order lifecycle
//! - `risk_manager` — Position sizing, drawdown limits, P&L tracking
//! - `order_executor` — Dhan REST API order submission with governor rate limiting
//!
//! # Boot Sequence Position
//! State -> **OMS -> Risk -> Execute** -> Persist
