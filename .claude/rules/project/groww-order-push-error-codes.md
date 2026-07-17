# Groww Order/Position PUSH Channel — Error Codes (GROWW-PUSH-01..04)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39 (the Groww order-side grant) +
> the 2026-07-15 scope-lock amendment (*"market data = per-minute REST pull,
> order/position events = live push"* — the permanently-kept live-push surface,
> SEPARATE from the retired live market-data feed) >
> `groww-shared-token-minter-2026-07-02.md` (SSM read-only; NEVER mint) >
> `no-rest-except-live-feed-2026-06-27.md` §10 (the order-side grant) > this file.
> **Scope:** the Groww ORDER-UPDATE + POSITION-UPDATE **live push** channels —
> NATS-over-WebSocket to `wss://socket-api.groww.in`, subjects
> `stocks_fo/order/updates.apex.<subscriptionId>` /
> `stocks_fo/position/updates.apex.<subscriptionId>` /
> `stocks/order/updates.apex.<subscriptionId>`. This channel RECEIVES broker
> order/position events; it places NO order. Live order-FIRE stays behind the
> unchanged 4-gate lattice + `GROWW_ORDER_LIVE_FIRE = false`. DEFAULT-OFF:
> nothing connects until `[groww_orders] order_push_enabled = true` AND the
> `groww_orders` cargo feature is built.
> **Companion contracts:** `crates/common/src/error_code.rs` (the 4
> `GrowwPush0*` variants), `crates/common/src/config.rs`
> (`GrowwOrdersConfig::order_push_enabled`), `crates/common/src/broker_order_events.rs`
> (the `BrokerOrderEvent` seam the order channel maps onto),
> `crates/common/src/ws_event_types.rs` (`WsType::GrowwOrderUpdate`).
> **Companion code (lands in later serial PRs of this build):**
> `crates/trading/src/oms/groww/push/{socket_token,nkey,nats,proto,subjects,connect,order_mapper,position,runner}.rs`,
> `crates/app/src/groww_order_observability.rs`.
> **Companion runbook:** `groww-orders-error-codes.md` (GROWW-ORD-08 is the
> order-audit-write code emitted by the push channel's audit consumer).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `GrowwPush0*` variant's code_str verbatim —
> `GROWW-PUSH-01`, `GROWW-PUSH-02`, `GROWW-PUSH-03` and `GROWW-PUSH-04` all
> appear below.

---

## §0. Why these codes exist (the shared contract)

The Groww order/position push channel is a native-Rust NATS-over-WebSocket
client under `crates/trading/src/oms/groww/push/` (the transport building
blocks — `nats.rs` framing, `nkey.rs` ed25519 codec, `socket_token.rs` mint,
`proto.rs` primitives — are restored/adapted from git history `dd7eaa5^`;
brutex code banned). It bootstraps a session (SSM-read access token → ephemeral
ed25519 nkey → `POST /v1/api/apex/v1/socket/token/create/` → NATS user JWT +
`subscriptionId`), CONNECTs to `wss://socket-api.groww.in`, SUBscribes to the
order/position subjects, and hand-decodes the proto3 `OrderDetailsBroadCastDto`
/ `PositionDetailProto` payloads. Order updates map to `BrokerOrderEvent` and
fan out to the executor's `GrowwOrderHintSink` seam + the order-audit consumer
(`crates/app/src/groww_order_observability.rs`, feed=`groww`, mode=`paper`).
Position updates are decoded, counted, and logged. Every failure class along
that path is one of the four codes below.

All four ship **LOG-SINK-ONLY** — there is NO `error-code-alarms.tf` entry and
NO `observability-architecture.md` paging-list mention (the paging drift guard
pins both surfaces untouched). The coded `error!` lines are the forensic WHY;
the channel adds NO new Telegram (the Dhan order-side noise-lock — typed order
events are the later wiring-cluster with its own rule edit). A push failure
NEVER gates a mutation and NEVER blocks the tick-free runtime.

The token is READ-ONLY from SSM and NEVER minted (`groww-shared-token-minter-2026-07-02.md`);
on any auth/JWT failure the client re-reads the SSM access token (paced ≥ 60 s)
and re-mints the per-session socket token — it does NOT mint the access token.

---

## §1. GROWW-PUSH-01 — push connect / reconnect failed

**Severity:** High. **Auto-triage safe:** Yes (bounded reconnect re-attempts;
the operator inspects, never manually reconnects first).

**Trigger:** `ErrorCode::GrowwPush01ConnectFailed` — the NATS-over-WS session
could not be established or an established session dropped for a
transport-class reason: the WS handshake to `wss://socket-api.groww.in` failed,
the `POST /v1/api/apex/v1/socket/token/create/` socket-token mint returned
non-2xx / malformed, the NATS `INFO`/`CONNECT`/nonce-sign exchange failed at the
transport level (auth-class CONNECT rejects are GROWW-PUSH-02), a SUB was
refused, or the socket closed / read-loop errored. The runner reconnects with
bounded exponential backoff (fresh keypair + fresh socket-token mint +
re-subscribe); a session surviving the stability window resets the ladder.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the `GROWW-PUSH-01` payload: the
   `stage` (`socket_token_mint` / `ws_handshake` / `nats_connect` / `subscribe`
   / `read_loop`) + a bounded secret-redacted failure sample.
2. `tv_groww_push_reconnects_total{stage}` rate — sustained reconnects during
   market hours mean Groww is rejecting the session; cross-check the shared
   token (GROWW-PUSH-02) and the ws_event_audit `WsType::GrowwOrderUpdate`
   disconnect rows.
3. Never re-mint the access token — only the per-session socket token is
   re-minted; the access token is the bruteX minter Lambda's job.

## §2. GROWW-PUSH-02 — socket-token mint / CONNECT auth rejected

**Severity:** High. **Auto-triage safe:** Yes (SSM re-read ladder; NEVER mint
the access token).

**Trigger:** `ErrorCode::GrowwPush02AuthFailed` — an auth-class failure: the
SSM access-token read failed, the socket-token mint returned 401/403 /
malformed, the server `INFO` carried no nonce, or the NATS `CONNECT` was
rejected with an auth-class `-ERR` (`authorization` / `authentication`).
Recovery: RE-READ `/tickvault/<env>/groww/access-token` from SSM at ≥ 60 s
pacing (never mint — token-minter lock), fresh ed25519 keypair, fresh
socket-token mint. At ~06:00 IST this is the daily token reset — self-heals
once the bruteX minter Lambda writes the fresh token.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the payload names the `auth_class` + the
   HTTP status or server `-ERR` text (never a token value).
2. Sustained 401 outside the ~06:00 IST window = the minter Lambda is stale —
   see the shared-token-minter runbook; NEVER mint from here.
3. `tv_groww_push_auth_failures_total{auth_class}` is the trend store.

## §3. GROWW-PUSH-03 — proto3 decode / framing failure

**Severity:** Medium. **Auto-triage safe:** Yes (skip + count; the read loop
continues).

**Trigger:** `ErrorCode::GrowwPush03DecodeFailed` — a delivered MSG payload
failed hand-written proto3 decode of `OrderDetailsBroadCastDto` /
`PositionDetailProto` (`kind="proto"`, counted + skipped), arrived on an
unexpected subject (`kind="unknown_subject"`), or the NATS text-protocol framing
hit a violation (`kind="framing"` — connection reset). One malformed frame never
stalls the channel.

**Triage:**
1. `tv_groww_push_decode_errors_total{kind}` — a low `proto` rate is optional-field
   schema drift (forward-skipped by design); a HIGH rate means Groww changed the
   order/position wire schema — re-extract the DTO definitions from the current
   wheel (`docs/groww-ref/` / the growwapi extract) and update the hand-written
   decoder.
2. `kind="framing"` means the server is not speaking core-NATS text protocol as
   expected — capture bytes via a local probe before changing the parser.
3. `kind="unknown_subject"` means a subject arrived that the subscription set did
   not request — cross-check the `subscriptionId` subject builders.

## §4. GROWW-PUSH-04 — push runner supervisor respawned

**Severity:** High. **Auto-triage safe:** Yes (the respawn already self-healed).

**Trigger:** `ErrorCode::GrowwPush04SupervisorRespawned` — the supervised push
runner task died (panic / unexpected return / external cancel) and the
respawning wrapper (the `disk_health_watcher.rs` template) caught it, logged
`error!`, incremented `tv_groww_push_supervisor_respawn_total{reason}`
(`panic` / `cancelled` / `clean_exit` / `unknown` via the shared
`classify_join_exit`), waited the bounded backoff, and respawned — so the
order/position channel can never die silently.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the `reason` field; for `reason="panic"`
   read the backtrace in `data/logs/errors.jsonl.*` immediately preceding.
2. A one-off respawn at shutdown is benign (`cancelled`). A sustained
   `tv_groww_push_supervisor_respawn_total` rate is a real bug — restart the app
   to reset from a clean state and file the backtrace.

**Honest envelope (panic):** the workspace release profile sets
`panic = "abort"`, so in the PRODUCTION binary a runner panic ABORTS the
process — the respawn arms self-heal in UNWIND (dev/test) builds only; release
recovery is process restart. No in-process panic self-healing is claimed for
release builds.

---

## §5. Delivery boundary (honest — no false-OK)

All four codes are **log-sink-only today**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s "Which codes page" list (the paging drift
guard sees no drift). The push channel adds NO new Telegram event and NO
`events.rs` edit (the Dhan order-side noise-lock). The order-audit-write failure
of the channel's consumer is `GROWW-ORD-08` (also log-sink-only —
`groww-orders-error-codes.md`). Adding a CloudWatch log-filter alarm for any
`GROWW-PUSH-*` code is a flagged follow-up (one `error_code_alerts` map entry +
a doc paragraph + a cost note — the SCOREBOARD-01 / FEED-REJECT-01 precedent).

**Honest envelope:** the channel is native-Rust NATS-over-WS restored from the
deleted market-data transport (`dd7eaa5^`); the order/position PROTO messages
and SUBJECTS are NEW (the deleted `proto.rs`/`subjects.rs` covered tick/depth/
index only), so the wire shape is UNVERIFIED-LIVE until the first authorized
session (defensive optional-field decode; a schema drift is LOUD via
GROWW-PUSH-03, never silent). No order is placed by this channel — it RECEIVES
events; live order-fire stays behind the unchanged 4-gate lattice
(`GROWW_ORDER_LIVE_FIRE = false`). Whether our Groww API key carries order/
position push ENTITLEMENT is UNPROBED (the first enabled session is the probe).

---

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwPush0*` variant)
- `crates/common/src/config.rs` (`GrowwOrdersConfig::order_push_enabled`) or
  `config/base.toml` `[groww_orders]`
- `crates/common/src/ws_event_types.rs` (`WsType::GrowwOrderUpdate`)
- Any future file under `crates/trading/src/oms/groww/push/`
- `crates/app/src/groww_order_observability.rs`
- Any file containing `GROWW-PUSH-01`, `GROWW-PUSH-02`, `GROWW-PUSH-03`,
  `GROWW-PUSH-04`, `GrowwPush01ConnectFailed`, `GrowwPush02AuthFailed`,
  `GrowwPush03DecodeFailed`, `GrowwPush04SupervisorRespawned`,
  `order_push_enabled`, `GrowwOrderUpdate`, `socket-api.groww.in`,
  `socket/token/create`, or `updates.apex.`
