# Implementation Plan: Groww Native-Rust Shadow Client (PR-R1 — parity migration step 1)

**Status:** VERIFIED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator) — "go", 2026-07-04, this session (native-Rust shadow client + new-dep authorization)

> **Authority:** `groww-second-feed-scope-2026-06-19.md` §32 (native Rust remains
> the production option) + §33 (LIVE-FEED-ONLY) + `groww-shared-token-minter-2026-07-02.md`
> (NEVER mint the access token) + the 2026-07-04 protocol extraction
> (growwapi-1.5.0 wheel; scratchpad `groww-native-rust-protocol.md`).
> **Per-item guarantee matrix:** cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row + 7-row) — the specifics for this PR are in the Design/Observability/Test Plan
> sections below; the honest 100% claim carries the §F envelope qualifier.

## Design

PR-R1 ships a **DEFAULT-OFF native-Rust Groww shadow client** that runs
alongside (never instead of) the Python sidecar. It connects to Groww's
NATS-over-WebSocket feed (`wss://socket-api.groww.in`), subscribes the SAME
watch set the sidecar reads (`data/groww/groww-watch-<date>.json`), decodes
ticks, and appends them to its OWN NDJSON file
`data/groww/rust-live-ticks.ndjson` in the exact `GrowwTickLine` schema the
bridge parses (`crates/app/src/groww_bridge.rs`) — so the future PR-R2 parity
comparer is a trivial keyed diff. NO shared-table writes, NO strategy/order
wiring, NO sidecar changes, NO historical calls (§33).

**Dependency decision (honest deviation, smaller than approved):** the operator
approved 3 new deps (`async-nats`, `nkeys`, `prost`). During implementation we
found the repo ALREADY has merged, tested, zero-dep native equivalents:
`feed/groww/nats.rs` (NATS framing parser + SUB/PONG builders),
`feed/groww/nkey.rs` (nkey base32+CRC16 codec), `feed/groww/proto.rs`
(zero-alloc protobuf decoder for the exact wheel schema), and
`feed/groww/subjects.rs` (subject grammar). Re-using them (a) avoids 3 new
supply-chain roots, (b) keeps FULL control of the CONNECT frame fields (the #1
live-probe risk is Groww accepting a non-nats-py client — `async-nats` does not
let us mimic nats-py's CONNECT), and (c) matches the task's own "prefer
hand-written prost structs" guidance. The ONLY dep change is promoting
**`aws-lc-rs` 1.17.0** (already resolved in Cargo.lock via rustls) to a direct
workspace dep for ed25519 session-keypair generation + nonce signing. Strictly
fewer deps than approved — flagged for coordinator/operator confirmation.

Components (all under `crates/core/src/feed/groww/native/`):
- `base64url.rs` — pure RFC 4648 URL-safe no-pad encoder (CONNECT `sig` field).
- `keypair.rs` — per-session ed25519 keypair (aws-lc-rs `generate_pkcs8` →
  `from_pkcs8`), public key → nkey `U…` string via `nkey.rs`, nonce signing.
  Seed material never exposed, never logged, zeroized with the pkcs8 buffer.
- `socket_token.rs` — the KEPT live-feed-AUTH REST call
  `POST https://api.groww.in/v1/api/apex/v1/socket/token/create/` with
  `Authorization: Bearer <access-token from SSM read-only>` (shared-minter lock
  untouched — we never mint the access token; socket-token minting is
  per-session NATS auth, recorded in the no-REST lock KEEP table). Hardened
  reqwest client (no redirects, bounded timeouts). Token + JWT are
  `SecretString` end to end.
- `connect.rs` — INFO nonce extraction + pure CONNECT frame builder
  (jwt + base64url(ed25519_sign(nonce))), nats-py-compatible field values.
- `watch_reader.rs` — deserialize the sidecar's watch file; build the
  subject → `(security_id, canonical_segment)` map (I-P1-11-safe: identity is
  the full subject string, which encodes exchange+segment+token).
- `shadow_writer.rs` — dedicated writer task behind a BOUNDED mpsc; NDJSON
  lines in the `GrowwTickLine` schema (`security_id, segment, ts_ist_nanos,
  exchange_ts_millis, ltp, cum_volume=0, capture_ns`); IST-midnight rotation to
  `rust-live-ticks-YYYYMMDD.ndjson` mirroring the sidecar's rotation.
- `client.rs` — the connector: tokio-tungstenite WS connect → INFO → CONNECT →
  SUB per instrument → read loop (`nats::parse_frame`, PONG on PING, decode via
  `proto.rs`, subject → identity map, `capture_ns` stamp, `try_send` to the
  writer). Two-tier recovery: transport reconnect with bounded exponential
  backoff (same socket JWT), auth-class failure (`-ERR Authorization
  Violation`) → fresh keypair + fresh socket-token mint (access token re-read
  from SSM, ≥60s pacing — never minted).
- `crates/app/src/groww_native_shadow.rs` — supervised spawn (WS-GAP-05
  pattern), gated on NEW config flag `feeds.groww_native_shadow` (default
  false); waits for today's watch file, runs the client, respawns on death.

## Edge Cases

- Flag ON but Groww lane OFF / watch file absent → the task polls for today's
  watch file (30s) and only connects once it exists; never builds its own
  universe (single source: the Rust-built watch file).
- IST midnight while connected → writer rotates the NDJSON at the day boundary
  (one integer compare per write); the client keeps streaming; the watch task
  re-checks the date and reconnects with the new day's watch file.
- MSG payload malformed / unknown fields → `proto.rs` skips or returns typed
  error; counted + logged (GROWW-NATIVE-03), never panics, never kills the loop.
- MSG for an unknown subject (server-side extra) → counted, skipped.
- Writer channel full (slow disk) → `try_send` drop counted loudly
  (shadow-only: dropping a shadow line never touches the production capture
  chain — the sidecar remains the durable capture path in R1).
- WS frame fragmentation → the read loop accumulates bytes in a buffer;
  `parse_frame` returns `Ok(None)` on incomplete frames (already tested).
- Groww rejects the non-nats-py CONNECT (live-probe unknown) → `-ERR` handled,
  bounded backoff + re-mint ladder, edge-triggered error log; the sidecar is
  unaffected (shadow isolation).
- Access token stale (06:00 IST reset) → socket-token mint 401 → re-read SSM
  at ≥60s pacing, never mint (token-minter lock).
- Non-trading day / feed disabled → the shadow task idles on the watch-file
  poll (no watch file for today = no connection).

## Failure Modes

| Failure | Detection | Action | Code |
|---|---|---|---|
| WS connect / handshake / read error | connect result / read loop `Err` | bounded expo backoff reconnect (5s→60s cap), edge-triggered `error!` | GROWW-NATIVE-01 |
| Socket-token mint failed / CONNECT auth rejected | HTTP non-2xx / `-ERR` auth class | fresh keypair + re-mint; SSM re-read ≥60s pacing; never mint access token | GROWW-NATIVE-02 |
| NATS/proto decode failure | typed parse errors | count + skip (proto) or reconnect (framing), `error!` coalesced | GROWW-NATIVE-03 |
| NDJSON write / rotation failure | io::Error in writer task | `error!` (meta-guard: persist failures are error!) + retry next write; capture NEVER stops for rotation | GROWW-NATIVE-04 |
| Shadow task death | supervisor JoinHandle | respawn with backoff (WS-GAP-05 pattern) | GROWW-NATIVE-01 (reason=respawn) |

Honest envelope: the shadow client is validation-only in R1 — its NDJSON is
not a durability floor (the sidecar + bridge chain remains the production
capture path). A shadow outage loses only comparison data for the window.

## Test Plan

- `base64url`: RFC 4648 vectors + empty + 64-byte signature length.
- `keypair`: public key is 32 bytes; nkey string starts with `U` and
  round-trips through `nkey::decode`; two sessions produce different keys;
  signature verifies via aws-lc-rs UnparsedPublicKey.
- `connect`: nonce extraction from INFO json (present/absent/garbage); CONNECT
  frame is one line, contains jwt + sig, correct terminator, never logs.
- `socket_token`: pure request-body builder shape (`{"socketKey": "U…"}`),
  header set (x-client-id etc.) — NO live calls; response parse.
- `watch_reader`: parses the exact output of `serialize_watch_file`
  (round-trip against the writer in `instruments.rs`); subject map covers
  Ltp + IndexValue kinds; duplicate subjects rejected/deduped.
- `shadow_writer`: NDJSON line format parity — serialize one record and
  field-compare against a checked-in literal sample of the Python sidecar line
  format; IST rotation naming pure fn; ms→IST-nanos conversion vectors.
- `client`: reconnect backoff pure fn table; auth-class `-ERR` classifier;
  tick-record assembly from a decoded proto + subject.
- config: `groww_native_shadow` defaults to false; absent key deserializes.
- error_code: existing ratchet tests (all()/code_str/runbook/severity) extended
  by the 4 new variants; cross-ref test satisfied by the new rule file.

## Rollback

`feeds.groww_native_shadow = false` is the default and the kill switch — the
task is never spawned; behaviour is byte-identical to today. Full rollback =
revert the single PR commit; no schema, no shared-table, no sidecar, no Dhan
surface is touched, so nothing needs data repair. The new dep (aws-lc-rs
direct) was already in the tree at the same version, so reverting restores the
exact prior dependency graph.

## Observability

- Counters: `tv_groww_native_ticks_captured_total`,
  `tv_groww_native_reconnects_total{reason}`,
  `tv_groww_native_decode_errors_total{kind}`,
  `tv_groww_native_writer_errors_total{stage}`,
  `tv_groww_native_dropped_total{reason}` (writer-channel full),
  `tv_groww_native_respawn_total`.
- 4 typed ErrorCodes (GROWW-NATIVE-01..04) with runbook
  `.claude/rules/project/groww-native-rust-error-codes.md`; every `error!`
  carries `code = ErrorCode::X.code_str()` (tag-guard).
- Edge-triggered logging on reconnect storms (never per-loop spam).
- No Telegram event in R1 (shadow-only, no operator action exists yet;
  the errors still reach the 5-sink chain via `error!`).
- No audit table in R1 (no typed lifecycle event beyond logs — the NDJSON file
  itself is the forensic record for the parity comparer; flagged N/A per
  zero-loss charter §3 "N/A — shadow validation artifact, no shared-table write").

## Plan Items

- [x] Item 1 — Workspace dep: promote `aws-lc-rs = "1.17.0"` to direct pin; core uses `{ workspace = true }`
  - Files: Cargo.toml, crates/core/Cargo.toml
  - Tests: (dependency-checker; cargo build)
- [x] Item 2 — Config flag `feeds.groww_native_shadow` default OFF
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_feeds_config_groww_native_shadow_defaults_off
- [x] Item 3 — Constants: socket URL, socket-token URL, shadow NDJSON path, backoff bounds
  - Files: crates/common/src/constants.rs
  - Tests: test_groww_native_constants_pinned
- [x] Item 4 — ErrorCode variants GROWW-NATIVE-01..04 + rule file
  - Files: crates/common/src/error_code.rs, .claude/rules/project/groww-native-rust-error-codes.md
  - Tests: existing error_code ratchets + error_code_rule_file_crossref
- [x] Item 5 — native module: base64url + keypair + connect + socket_token
  - Files: crates/core/src/feed/groww/native/{mod,base64url,keypair,connect,socket_token}.rs
  - Tests: test_encode_no_pad_rfc4648_vectors, test_nkey_public_roundtrips_via_decoder, test_sign_nonce_b64url_verifies, test_build_connect_frame_shape, test_socket_token_request_body
- [x] Item 6 — native module: watch_reader + shadow_writer + client
  - Files: crates/core/src/feed/groww/native/{watch_reader,shadow_writer,client}.rs
  - Tests: test_parse_watch_file_and_build_subject_map, test_watch_file_roundtrip_with_serializer, test_format_ndjson_line_matches_python_sidecar_schema, test_archive_path_for_day_naming, test_reconnect_delay_secs_table, test_ms_to_ist_nanos
- [x] Item 7 — app wiring: supervised shadow task gated on the flag
  - Files: crates/app/src/groww_native_shadow.rs, crates/app/src/main.rs, crates/app/src/lib.rs (if module list)
  - Tests: test_watch_file_path_for_matches_builder_naming, test_secs_until_day_roll, test_feeds_config_groww_native_shadow_defaults_off (the boot gate is the config flag)
- [x] Item 8 — docs/rules: §35 dated note in groww-second-feed-scope; KEEP row for the socket-token endpoint in no-rest lock; async-nats stale-claim fix in docs/groww-ref/02-verified-endpoints.md
  - Files: .claude/rules/project/groww-second-feed-scope-2026-06-19.md, .claude/rules/project/no-rest-except-live-feed-2026-06-27.md, docs/groww-ref/02-verified-endpoints.md
  - Tests: (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Flag off (default) | No task spawned; byte-identical behaviour |
| 2 | Flag on, watch file present, Groww healthy | Shadow NDJSON grows alongside sidecar NDJSON, same schema |
| 3 | Groww rejects CONNECT | Bounded backoff + re-mint ladder; sidecar unaffected |
| 4 | IST midnight | Shadow file rotates to dated archive; capture continues |
| 5 | Disk write fails | error! GROWW-NATIVE-04, retry next write, client keeps streaming |
| 6 | Decode garbage payload | Counted skip; no panic; loop continues |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the shadow
client is default-OFF (config-gated, one boot-time read); every parse path is
panic-free typed-error code (bounds-checked framing + proto decoders, unit +
adversarial tested); reconnect and re-mint ladders are bounded-backoff pure
functions with tests; the production Dhan + Groww-sidecar capture chains are
untouched (no shared-table writes, no new tick-drop path). NOT claimed: that
Groww's server accepts a non-nats-py client (live-probe unknown — the first
enabled run answers it), nor that the shadow NDJSON is a durability floor (it
is a validation artifact; the sidecar chain remains the capture path in R1).
