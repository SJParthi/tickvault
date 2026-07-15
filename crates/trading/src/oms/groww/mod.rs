//! Groww order-side submodule — the GATED broker order lane (operator
//! authorization 2026-07-14).
//!
//! # Live-fire lattice (§39.2)
//! This entire subtree is Gate 2 of the 4-gate live-fire lattice: it compiles
//! ONLY under the NON-DEFAULT `groww_orders` cargo feature. A default build
//! (the shipped prod + CI default) does not include this module at all, so
//! there is no Groww order code in the binary until the feature is explicitly
//! enabled. The other three gates are: config default-OFF (`[groww_orders]`,
//! Gate 1), the hardcoded `tickvault_common::constants::GROWW_ORDER_LIVE_FIRE`
//! const (Gate 3), and the rule lock (§39 / §10, Gate 4). Mutating order
//! requests stay refused until ALL four align via a future dated operator
//! live-orders-enable.
//!
//! # Area collision contract (§39.3 — one owner per file, serial area PRs)
//! Each order-side area lands as its OWN file on a non-overlapping path with
//! its own error-code prefix, so area PRs never touch the same file:
//!
//! | Area                | File (this subtree)                    | Error codes  |
//! |---------------------|----------------------------------------|--------------|
//! | Orders              | `oms/groww/api_client.rs`              | `GROWW-ORD-*`  |
//! | Smart Orders (OCO)  | `oms/groww/smart_orders.rs`            | `GROWW-OCO-*`  |
//! | Portfolio           | `oms/groww/portfolio.rs`               | `GROWW-PORT-*` |
//! | Margin              | `oms/groww/margin.rs`                  | `GROWW-MARG-*` |
//! | User + Exceptions   | `oms/groww/user.rs`                    | `GROWW-READY-*` (readiness reuses the existing SPOT1M/CHAIN codes per its design) |
//!
//! Shared files (`crates/common/src/error_code.rs`, `config.rs`,
//! `config/base.toml`, `crates/core/src/notification/events.rs`,
//! `oms/mod.rs`, and THIS `oms/groww/mod.rs`) are touched SERIALLY by the
//! build lead only — never by two area PRs at once.
//!
//! # Shared seam
//! Every area maps its broker payloads onto the neutral
//! [`tickvault_common::broker_order_events::BrokerOrderEvent`] /
//! [`BrokerOrderStatus`] (with [`EventSource`] naming the transport
//! provenance — REST status poll vs order-update stream push) so the OMS,
//! audit, and reconciliation speak one order shape across brokers.
//! Re-exported here for the area modules' convenience.
//!
//! PR-0 ships this stub ONLY — no area module exists yet; the `pub use`
//! below is the sole surface, and it introduces no order request.

pub use tickvault_common::broker_order_events::{BrokerOrderEvent, BrokerOrderStatus, EventSource};

// Orders-area PURE CORE (ORD-PR-2 — GROWW-ORD-*; no transport, no ErrorCode
// emits yet — those land in ORD-PR-3 once the variants exist):
pub mod intent_ledger;
pub mod poll_tiers;
pub mod reconcile;
pub mod reference_id;
pub mod state;
pub mod types;

// Area modules land in their own serial PRs, each behind this same feature:
//   pub mod api_client;    // Orders        (GROWW-ORD-*)
//   pub mod smart_orders;  // Smart Orders  (GROWW-OCO-*)
//   pub mod margin;        // Margin        (GROWW-MARG-*)
//   pub mod user;          // User + Exceptions (GROWW-READY-*)

/// Portfolio area (`GROWW-PORT-*`) — field-inventory probe (item 6c.2).
pub mod portfolio;
