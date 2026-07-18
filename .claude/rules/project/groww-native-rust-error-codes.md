---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/core/src/feed/groww/native/**"
  - "crates/app/src/groww_native_shadow.rs"
---

# Groww Native-Rust Shadow Client — Error Codes (GROWW-NATIVE-01..04)

> **⚠ RETIRED 2026-07-15 (Groww live feed deleted — operator Q1, received directly in this session: *"remove
> the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."*):** the native-Rust
> shadow client, its supervised runner, and every GROWW-NATIVE-01..04 emit site are DELETED with the Groww
> live feed (the PR-R2/R3 parity migration is moot — there is no live feed to migrate to). The
> `GrowwNative0*` ErrorCode variants are RETAINED until the post-C4 variant sweep (this file keeps
> satisfying the cross-ref test). Content below retained as historical audit.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §32/§33/§35 (native Rust is the
> production option; LIVE-FEED-ONLY; the 2026-07-04 shadow authorization) >
> `groww-shared-token-minter-2026-07-02.md` (NEVER mint the access token) >
> this file.
> **Operator authorization (2026-07-04, "go", this session):** PR-R1 of the
> parity migration — the native-Rust client runs DEFAULT-OFF in SHADOW
> alongside the Python sidecar, writing its own NDJSON capture for the future
> exact per-tick parity comparer. Protocol source: the official
> `growwapi-1.5.0` wheel extraction (see `docs/groww-ref/`; brutex code
> banned).
> **Companion code:** `crates/core/src/feed/groww/native/` (base64url /
> keypair / connect / socket_token / watch_reader / shadow_writer / client),
> `crates/app/src/groww_native_shadow.rs` (supervised runner),
> `crates/common/src/error_code.rs::ErrorCode::{GrowwNative01ConnectFailed,
> GrowwNative02AuthFailed, GrowwNative03DecodeFailed,
> GrowwNative04WriterFailed}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `GrowwNative0*` variant verbatim —
> `GROWW-NATIVE-01`, `GROWW-NATIVE-02`, `GROWW-NATIVE-03`, `GROWW-NATIVE-04`
> appear below.

---

## §0. Why these codes exist (the shadow contract)

The shadow client is a VALIDATION artifact: it must be loud about every
failure class (connect, auth, decode, write) so the parity run's coverage is
honest — but it is NOT a durability floor. Every code below is
Severity::Medium + auto-triage-safe because a shadow failure loses ONLY
comparison data for the window; the production capture chains (Dhan WS + the
Groww Python sidecar → bridge) are untouched by construction (no shared-table
writes, no shared tasks, its own NDJSON file). A sustained rate on any of
these counters during an enabled run IS the live-probe answer the shadow run
exists to collect.

## §1. GROWW-NATIVE-01 — connect / disconnect / runner respawned

**Severity:** Medium. **Auto-triage safe:** Yes (bounded backoff reconnect +
supervisor respawn already self-heal).

**Trigger:** the WebSocket connect to `wss://socket-api.groww.in` failed, an
established session ended transport-class (socket closed / IO error /
framing-buffer cap), or the supervised runner task itself died and was
respawned (`reason` label on `tv_groww_native_respawn_total`). Reconnect is
bounded expo backoff (`GROWW_NATIVE_RECONNECT_BASE_SECS` 5s →
`GROWW_NATIVE_RECONNECT_MAX_SECS` 60s cap); a session that completes its
handshake resets the ladder.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-NATIVE-01`; the payload
   carries `attempt` + `wait_secs` (or `reason` for a respawn).
2. `tv_groww_native_reconnects_total{reason}` — `transport` sustained during
   market hours while the SIDECAR is healthy = Groww rejecting the native
   client specifically (the live-probe unknown) — record the finding; the
   sidecar keeps capturing.
3. Both native AND sidecar down → provider-side; see FEED-STALL-01.

## §2. GROWW-NATIVE-02 — socket-token mint / CONNECT auth rejected

**Severity:** Medium. **Auto-triage safe:** Yes (re-key + re-mint ladder).

**Trigger:** the per-session socket-token mint (`GROWW_SOCKET_TOKEN_URL`,
live-feed-AUTH KEEP class per `no-rest-except-live-feed-2026-06-27.md` §3)
returned non-2xx / malformed, the SSM access-token read failed, the server
`INFO` carried no nonce, or the NATS `CONNECT` was rejected with an
auth-class `-ERR` (`authorization` / `authentication`). Recovery: fresh
ed25519 keypair + fresh mint; on 401/403 the ACCESS token is RE-READ from SSM
at `GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS` (60s) pacing — NEVER minted
(token-minter lock 2026-07-02; the minter Lambda owns the daily ~06:05 IST
mint).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the payload carries `auth_class` +
   the HTTP status or server `-ERR` text (never a token value).
2. At ~06:00 IST this is the daily token reset — self-heals once the minter
   Lambda writes the fresh token.
3. Sustained 401 with a HEALTHY sidecar = the native client's mint headers /
   CONNECT shape are being rejected (live-probe finding) — capture the status
   + `-ERR` text verbatim for the protocol notes.

## §3. GROWW-NATIVE-03 — NATS framing / protobuf decode failure

**Severity:** Medium. **Auto-triage safe:** Yes (skip-or-reconnect already
applied).

**Trigger:** a MSG payload failed protobuf decode (counted + skipped —
`tv_groww_native_decode_errors_total{kind="proto"}`), a MSG arrived on an
unknown subject (`kind="unknown_subject"`), the NATS framing parser hit a
protocol violation (`kind="framing"` — connection reset), the framing buffer
exceeded its 4 MiB cap, or the daily watch file was unusable (fail-closed —
the client never subscribes a guessed universe).

**Triage:**
1. `tv_groww_native_decode_errors_total{kind}` — `proto` at a low rate is
   schema drift on optional fields (forward-skipped by design); a HIGH rate
   means Groww changed the wire schema — re-extract from the current wheel.
2. `framing` means the server is not speaking core-NATS text protocol as
   expected (or HMSG appeared — the CONNECT advertises `headers:false`
   precisely to prevent that); capture bytes via a local probe before
   changing the parser.
3. Watch-file errors: check the Groww lane activation built today's
   `data/groww/groww-watch-<date>.json` (the sidecar reads the same file —
   if BOTH consumers idle, the builder is the problem).

## §4. GROWW-NATIVE-04 — shadow NDJSON writer failed

**Severity:** Medium. **Auto-triage safe:** Yes (retry-next-write /
rotation-retry-next-midnight already applied).

**Trigger:** the shadow capture file (`data/groww/rust-live-ticks.ndjson`)
failed open/write, the IST-midnight rotation rename failed (capture continues
on the current file; retry next midnight — sidecar-identical semantics), or
the bounded writer channel overflowed (`tv_groww_native_dropped_total{reason=
"writer_full"}`). Per the error-level meta-guard, all persist-class failures
here log at `error!` with the code field.

**Triage:**
1. `df -h data/groww/` + `ls -la data/groww/` — disk full / permissions are
   the dominant causes.
2. `tv_groww_native_writer_errors_total{stage}` (`open` / `write`) rate — a
   sustained rate with healthy disk = check the file wasn't replaced by a
   directory / mount oddity.
3. Channel-full drops (a slow disk) lose SHADOW lines only; the parity
   comparer will show them as missing-in-rust — honest, never silent.

## §5. Honest envelope (§F wording)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> shadow client is default-OFF (config kill switch, `#[serde(default)]`);
> every parse path is panic-free typed-error code (bounds-checked NATS
> framing + zero-alloc protobuf decode, unit + adversarial tested); the
> reconnect and re-key/re-mint ladders are bounded pure functions with
> tests; the production Dhan + Groww-sidecar capture chains are untouched
> (no shared-table writes, no new tick-drop path). NOT claimed: that Groww's
> server accepts a non-nats-py client (the first enabled run answers it),
> nor that the shadow NDJSON is a durability floor — it is a validation
> artifact; the sidecar chain remains the capture path until PR-R3 cutover
> with a fresh dated operator quote."

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwNative0*` variant)
- `crates/core/src/feed/groww/native/` (any file)
- `crates/app/src/groww_native_shadow.rs`
- Any file containing `GROWW-NATIVE-`, `GrowwNative0`,
  `groww_native_shadow`, `rust-live-ticks.ndjson`,
  `run_native_shadow_client`, or `GROWW_SOCKET_TOKEN_URL`
