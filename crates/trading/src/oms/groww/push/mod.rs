//! Groww order/position PUSH transport (NATS-over-WebSocket) — Stage B of the
//! order-push build (operator authorization 2026-07-16, rule files
//! `.claude/rules/project/` on this branch; error codes `GROWW-PUSH-01..04`
//! landed in Stage A).
//!
//! # What this module is
//! The RESTORED, merged, previously-tested NATS-over-WS transport from the
//! retired Groww live market-data feed (deleted 2026-07-15 with the live feed
//! — recovered from `dd7eaa5e^:crates/core/src/feed/groww/`), re-homed here
//! as the ORDER/POSITION push channel per the operator's 2026-07-15 order-side
//! directive: market data = per-minute REST pull; order/position events =
//! live push. RECEIVE-ONLY, paper-mode, default-OFF:
//!
//! - Gate: this entire subtree compiles ONLY under the non-default
//!   `groww_orders` cargo feature (the `#[cfg(feature = "groww_orders")]`
//!   gate on `pub mod groww;` in `oms/mod.rs` — Gate 2 of the §39.2 lattice);
//!   the runtime consumer is additionally config-gated by
//!   `[groww_orders] order_push_enabled` (default OFF, Stage A).
//! - No order mutation exists anywhere in this module — it mints a
//!   per-session socket token, performs the NATS `INFO`→`CONNECT` handshake,
//!   frames/parses NATS protocol bytes, and decodes the order/position
//!   protobuf payloads. Stage C adds the RECEIVE-ONLY supervised runner
//!   ([`runner`]) — the Stage D app consumer wires its spawn behind the
//!   `order_push_enabled` config gate; nothing in this crate spawns it.
//!
//! # Module layout (10 files)
//! | File | Contents |
//! |---|---|
//! | [`nats`] | bounds-checked NATS text-protocol framing parser + frame builders |
//! | [`nkey`] | nkey codec (base32 + CRC16) + base64url + per-session ed25519 keypair |
//! | [`connect`] | `INFO` nonce extraction + `CONNECT` frame builder (JWT as secret) |
//! | [`socket_token`] | per-session socket-token mint (`POST /v1/api/apex/v1/socket/token/create/`) |
//! | [`subjects`] | order/position update subject builders (`…updates.apex.<subscriptionId>`) |
//! | [`proto`] | protobuf decoders for `OrderDetailsBroadCastDto` + `PositionDetailProto` |
//! | [`order_mapper`] | pure total mapper: `OrderDetailsBroadCastDto` → the neutral `BrokerOrderEvent` seam (Stage C) |
//! | [`position`] | position updates: decode + count + log + best-effort full-fidelity capture (Stage C; capture 2026-07-18) |
//! | [`order_events`] | full-fidelity capture builders + the bounded [`GrowwPushCapture`] producer sink (`ORDER-EVT-01`, 2026-07-18) |
//! | [`runner`] | the supervised bootstrap + read-loop runner (`GROWW-PUSH-01..04` taxonomy; Stage C) |
//!
//! # Secrets
//! The SSM-read access token and the minted NATS user JWT are
//! [`secrecy::SecretString`] end to end — never logged, never in a URL, never
//! `Debug`-printed. The access token is supplied by the CALLER (read-only SSM
//! path per `groww-shared-token-minter-2026-07-02.md`); this module never
//! mints an access token.
//!
//! # Shared error types
//! Each sub-module owns its typed, no-panic error enum, re-exported here so
//! the future runner speaks one surface: [`NatsParseError`], [`NkeyError`],
//! [`KeypairError`], [`SocketTokenError`], [`ProtoDecodeError`].

pub mod connect;
pub mod nats;
pub mod nkey;
pub mod order_events;
pub mod order_mapper;
pub mod position;
pub mod proto;
pub mod runner;
pub mod socket_token;
pub mod subjects;

pub use nats::NatsParseError;
pub use nkey::{KeypairError, NkeyError};
pub use proto::ProtoDecodeError;
pub use socket_token::SocketTokenError;

// Stage C (2026-07-16): the mapper + position handler + supervised runner
// surface the future Stage D app consumer speaks.
pub use order_mapper::{map_order_broadcast, map_order_detail};
pub use position::{PositionHandleOutcome, handle_position_payload};
pub use runner::{GrowwAccessTokenProvider, run_groww_push_supervised};

// Full-fidelity capture lane (ORDER-EVT-01, 2026-07-18): the bounded
// producer sink the app consumer constructs + the pure record builders.
pub use order_events::{
    GrowwPushCapture, build_groww_order_event_record, build_groww_position_event_record,
};
