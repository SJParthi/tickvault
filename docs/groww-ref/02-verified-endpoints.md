# Groww trade-api — verified endpoints (research record)

> **Status:** research findings, 2026-06-19. Recorded so future native-Rust work
> starts from facts, not guesses. **No hallucination:** every value carries a
> source; anything uncited is marked NOT FOUND.
> **Companion:** `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §32.

## Verified values

| Item | Value | Source | Confidence |
|---|---|---|---|
| Auth REST (access token) | `POST https://api.groww.in/v1/token/api/access` — `Authorization: Bearer <api_key>`, `x-api-version: 1.0`, body `{"key_type":"totp","totp":"<code>"}` → `{token}` | Groww cURL docs + 3rd-party SDK `config.ts` `AUTH_URL` | High — **matches our shipped `crate::feed::groww::auth`** |
| Live-feed WS URL | `wss://socket-api.groww.in` | `NithinSGowda/growwapi` `src/config.ts` `SOCKET_URL` | High |
| Transport | **NATS over WebSocket** + JWT/nkey auth (NOT raw WS) | SDK `nats.ws` `connect()` + `jwtAuthenticator`; Groww Python docs note "NATS" | High |
| Encoding | **Protobuf** (`liveFeed.proto` / `orderUpdate.proto`) | SDK `protobufjs`; Groww Python docs note "Protobuf" | High |
| Socket-token endpoint | `POST https://api.groww.in/v1/api/apex/v1/socket/token/create` → socket JWT/nkey creds | SDK `config.ts` `SOCKET_TOKEN_URL` | High |
| Subscribe model | NATS-subscribe per-instrument subjects (topic = exchange+segment+exchange_token) after JWT auth | SDK `liveFeed.ts` `generateSubscriptionTopic` | High (NodeJS) |
| `.proto` schema + exact subject grammar | **NOT FOUND** — binary assets inside the PyPI `growwapi` wheel | — | — |

## Caveats (honest)
- WS/transport/encoding evidence is from the **third-party** `NithinSGowda/growwapi`
  NodeJS SDK, **not** Groww-official source (Groww publishes none). Groww's own
  Python docs independently corroborate **NATS + Protobuf**.
- The exact `.proto` field definitions and NATS subject strings are **not in any
  citable open source** — they're embedded in the wheel. To build a native Rust
  client we must unpack the wheel to extract `liveFeed.proto`.

## Implication for native Rust (future)
- The URL + transport + auth flow are now known → native is **de-risked**.
- ~~BUT: `async-nats` is **TCP-only** — it does NOT speak NATS-over-WebSocket. A
  native client needs a NATS-over-WS layer (no mature Rust crate today) +
  nkey/JWT auth + the extracted `.proto`. This is weeks, not days.~~
  **STALE — corrected 2026-07-04:** `async-nats` has supported WebSocket
  transport since **v0.38.0** (feature `websockets`, default-on; 0.49.1
  current as of 2026-06-04 — verified via crates.io + the nats.rs CHANGELOG
  during the 2026-07-04 wheel re-extraction). The schemas were also fully
  decoded from the wheel's embedded descriptors, superseding the ".proto NOT
  FOUND" note above. PR-R1 (operator "go" 2026-07-04) shipped the native
  shadow client anyway on the repo's OWN zero-dep primitives
  (`crates/core/src/feed/groww/{nats,nkey,proto,subjects}.rs` + tungstenite)
  — see `groww-second-feed-scope-2026-06-19.md` §35 for the dependency
  decision record.
- Therefore: **Python SDK remains the production capture path; the native
  Rust client runs DEFAULT-OFF in shadow (PR-R1) toward the parity-gated
  cutover (PR-R3).**

## Sources
- [NithinSGowda/growwapi (NodeJS, 3rd-party)](https://github.com/NithinSGowda/growwapi) — `src/config.ts`, `src/utils/LiveFeed/index.ts`, `src/utils/Protobuffer/protobuffer.ts`
- [Groww cURL docs](https://groww.in/trade-api/docs/curl)
- [Groww Python SDK Feed docs](https://groww.in/trade-api/docs/python-sdk/feed)
- [growwapi · PyPI](https://pypi.org/project/growwapi/)
