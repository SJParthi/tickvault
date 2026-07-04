//! Groww NATIVE-RUST shadow client (PR-R1 of the parity migration — operator
//! "go" 2026-07-04; `groww-second-feed-scope-2026-06-19.md` §35).
//!
//! This is the "connector slice" the protocol primitives anticipated: it
//! composes the ALREADY-MERGED zero-dep building blocks —
//! [`crate::feed::groww::nats`] (framing), [`crate::feed::groww::nkey`]
//! (nkey codec), [`crate::feed::groww::proto`] (tick decode) and
//! [`crate::feed::groww::subjects`] (subject grammar) — into a live
//! NATS-over-WebSocket client that runs DEFAULT-OFF, in SHADOW, alongside the
//! Python sidecar:
//!
//! ```text
//! SSM access-token (read-only)          data/groww/groww-watch-<date>.json
//!        │                                          │
//!        ▼                                          ▼
//! socket-token mint (REST, KEEP class)      watch_reader → subject map
//!        │  {jwt, subscriptionId}                    │
//!        ▼                                          ▼
//! wss://socket-api.groww.in ── INFO → CONNECT(jwt, sig) → SUB per subject
//!        │
//!        ▼ MSG frames (protobuf)
//! proto decode → subject → (security_id, segment) → bounded mpsc
//!        │
//!        ▼
//! shadow_writer → data/groww/rust-live-ticks.ndjson (GrowwTickLine schema)
//! ```
//!
//! Scope locks honored: LIVE-FEED-ONLY (§33 — zero historical calls); the
//! access token is NEVER minted (shared-minter lock 2026-07-02 — SSM read
//! only); NO shared-table writes, NO strategy/order wiring, NO sidecar
//! changes. The Python sidecar remains the production capture path in R1;
//! this client's NDJSON exists solely for the PR-R2 exact per-tick parity
//! comparer.

pub mod base64url;
pub mod client;
pub mod connect;
pub mod keypair;
pub mod shadow_writer;
pub mod socket_token;
pub mod watch_reader;
