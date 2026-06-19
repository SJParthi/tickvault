//! Pluggable market-data feeds.
//!
//! Feed #1 = **Dhan** (the existing system, in [`crate::websocket`]).
//! Feed #2 = **Groww** ([`groww`], operator lock 2026-06-19 —
//! `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`).
//!
//! Groww is **native tickvault Rust** — brutex is a design/protocol reference
//! only, no code pulled — and reuses the same WAL/ring/spill/DLQ/aggregator
//! chain as Dhan. It is **default OFF** (`feeds.groww_enabled`); nothing here
//! runs unless the operator enables it, so the Dhan path is unchanged.

pub mod groww;
