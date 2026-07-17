# Implementation Plan: Groww Order-Update + Position-Update PUSH Channel

**Status:** APPROVED
**Date:** 2026-07-16
**Approved by:** Parthiban (operator), direct in-thread confirmation 2026-07-16 (thread event `f25b4dec-16b3-4a85-825a-85d2187b8011`): "implement and integrate everything" + "make it automated". Scope: BUILD + INTEGRATE the Groww order/position push channels, DEFAULT-OFF; live order-fire stays a separate future dated flip; the retired live market-data feed is untouched.
**Crates touched:** `common`, `trading`, `app` (STAGE 0 = docs only; no `crates/*/src/*.rs` logic change).
**Shared contract:** `/tmp/claude-0/-home-user-tickvault/e148fc19-1297-5a25-ada5-86d4f1af09ad/scratchpad/push-contract.md`
**Runbook:** `.claude/rules/project/groww-order-push-error-codes.md` (GROWW-PUSH-01..04).
**Deferred-spec producer:** this build is the producer the `groww-order-audit-consumer-spec` was waiting on.

---

## Pre-merge follow-up (BINDING — do not merge without it)

The three scope-lock files
(`.claude/rules/project/websocket-connection-scope-lock.md`,
`no-rest-except-live-feed-2026-06-27.md`,
`groww-second-feed-scope-2026-06-19.md`) already CARVE OUT order/position live
push as a permanently-kept surface SEPARATE from the retired market-data feed
(2026-07-15 amendment: *"market data = per-minute REST pull, order/position
events = live push"*), but they do NOT yet name THIS build's config key
(`order_push_enabled`), module tree (`oms/groww/push/`), or error codes
(GROWW-PUSH-01..04). Applying those line-edits is a deliberate operator/reviewer
follow-up — this automated session leaves the three scope-lock files UNTOUCHED.
**The PR ships DRAFT, NOT merged**, until that follow-up line-edit lands. The
operator authorization is recorded here in this approval line ONLY (standard
plan-enforcement), never in a scope-lock file.

---

## Design

Build a native-Rust NATS-over-WebSocket client (`crates/trading/src/oms/groww/push/`,
under the EXISTING `groww_orders` cargo feature) that RECEIVES Groww order-update
and position-update events over `wss://socket-api.groww.in`, maps order updates
onto the always-compiled `BrokerOrderEvent` seam, and fans them out to (a) the
executor's `GrowwOrderHintSink` and (b) a new order-audit consumer
(`crates/app/src/groww_order_observability.rs`, feed=`groww`, mode=`paper`, reusing
`OrderAuditWriter` AS-IS). Position updates are decoded, counted, and logged. The
channel places NO order; live order-fire stays behind the unchanged 4-gate lattice
(`GROWW_ORDER_LIVE_FIRE = false`). DEFAULT-OFF behind a new
`GrowwOrdersConfig::order_push_enabled` (serde default false) + the `groww_orders`
feature — nothing connects on a default build.

Wire protocol (verified from the growwapi-1.5.0 wheel extract + git history):
session bootstrap = SSM-read access token → ephemeral ed25519 nkey →
`POST /v1/api/apex/v1/socket/token/create/` → NATS user JWT + `subscriptionId`;
CONNECT signs the server nonce with the nkey seed; SUB the subjects
`stocks_fo/order/updates.apex.<subscriptionId>` (+ position + equity variants);
hand-decode proto3 `OrderDetailsBroadCastDto` / `PositionDetailProto` (order
prices int64 PAISE). The transport building blocks (`nats.rs`, `nkey.rs`,
`socket_token.rs`, proto3 primitives) are RESTORED/adapted from git history
`dd7eaa5^` (`4a8f446c50afea107067398633c9da8dd584f549`) — brutex code banned;
the order/position DTOs + subjects are NEW. Full field-by-field mapping tables +
module tree in the shared contract §1–§2.

Serial stages (impl): A contracts (4 ErrorCode variants + `order_push_enabled` +
`WsType::GrowwOrderUpdate` + lattice-guard Gate-1/reshape) → B transport under
`oms/groww/push/` (restore + `tokio-tungstenite` new-consumer) → C mapper + runner
+ multi-sink fan-out → D app wiring (`groww_order_observability.rs`, main.rs spawn,
app `groww_orders` feature). Each is its own serial PR (pr-completion-protocol).

## Edge Cases

- **Poll-skip / status lattice:** the push feed carries the SAME `orderStatus`
  enum the poller's rank-monotone lattice handles; a push event never bypasses the
  lattice — it feeds `BrokerOrderEvent`, and the existing state machine
  (`oms/groww/state.rs`) owns transition legality. Terminal-with-fill still fires
  the existing "position exists" signal.
- **Daily ~06:00 IST token reset:** surfaces as an auth-class CONNECT reject →
  GROWW-PUSH-02 → SSM re-read (paced ≥ 60 s, NEVER mint) → fresh socket-token
  mint → reconnect; self-heals once the bruteX minter Lambda writes the new token.
- **Optional-field / schema drift:** hand-written proto3 decode forward-skips
  unknown/absent optional fields; a HARD decode failure is GROWW-PUSH-03 (skip +
  count), never a silent gap and never a crash.
- **Reconnect metronome:** a server that closes each session just past the
  stability window pins the backoff at the base — bounded, loud (one GROWW-PUSH-01
  per cycle), never a tight kill loop.
- **DEFAULT-OFF safety:** on `order_push_enabled = false` OR a non-`groww_orders`
  build, no push code runs, no socket opens, byte-identical to today.
- **Live-fire independence:** the push channel is receive-only — no code path
  places an order; the 4-gate lattice + `GROWW_ORDER_LIVE_FIRE = false` are
  untouched, so enabling push can never place a live order.
- **Position-update ambiguity:** BSE/NSE credit/debit doubles are decoded +
  counted + logged only in this build (no position-table write); the reconcile
  surface is the portfolio-area session's follow-up (OPEN ITEM, contract §2.6).

## Failure Modes

| Failure | Code / handling |
|---|---|
| WS handshake / socket-token mint / NATS connect / SUB / read-loop drop | `GROWW-PUSH-01` (High, log-sink) + bounded reconnect + `tv_groww_push_reconnects_total{stage}` |
| SSM read / 401-403 mint / no-nonce / auth-class `-ERR` | `GROWW-PUSH-02` (High, log-sink) + SSM re-read ≥60 s (NEVER mint) + `tv_groww_push_auth_failures_total{auth_class}` |
| proto3 decode / unknown subject / framing violation | `GROWW-PUSH-03` (Medium, log-sink) + skip + `tv_groww_push_decode_errors_total{kind}` |
| runner task death (panic/cancel/return) | `GROWW-PUSH-04` (High, log-sink) + supervised respawn + `tv_groww_push_supervisor_respawn_total{reason}` |
| order-audit write (append/flush/sink_drop) | `GROWW-ORD-08` (Medium, log-sink; EXISTING code) — never gates anything |
| ws_event_audit row write | `AUDIT-WS-01` (EXISTING, best-effort) — never a supervision stall |
| release-build panic | `panic = "abort"` → process abort → restart recovery (honest envelope; respawn arms are unwind-build only) |

## Test Plan

- **Unit (trading, `--features groww_orders`):** proto3 decode of
  `OrderDetailUpdateDto` / `PositionDetailProto` (golden bytes from the wheel
  extract + optional-field-absent + garbage → skip); `from_groww_status` mapping of
  every `orderStatus` enum name; NATS CONNECT-frame nonce-sign roundtrip; subject
  builders for the 3 templates; the `OrderDetailUpdateDto → BrokerOrderEvent`
  mapper (paise, filled/remaining, segment, ts).
- **Unit (common):** the 4 new ErrorCode variants — `code_str` uniqueness +
  roundtrip, severity, `runbook_path` resolves on disk, `all()` length ratchet;
  `WsType::GrowwOrderUpdate` `as_str()`.
- **Consumer (app, `--features groww_orders`):** the second `GrowwOrderHintSink`
  impl `try_send` → Full/Closed → `GROWW-ORD-08 sink_drop`; the mapper →
  `OrderAuditRow` (feed=groww, mode=paper, best-fit event, `-1`/`n/a` sentinels).
- **Ratchets:** `error_code_rule_file_crossref.rs` (all 4 code_strs mentioned in
  the new runbook, runbook path exists); `groww_order_lattice_guard.rs` (Gate-1
  extended for `order_push_enabled = false`; Gate-3 `GROWW_ORDER_LIVE_FIRE`
  unchanged; Gate-5 passes — code under `/oms/groww/`, no Python literals).
- **Supervised-respawn:** `classify_join_exit` arms; a panicking inner task in an
  unwind build respawns + increments the counter.
- **Adversarial 3-agent review** (hot-path + security + hostile) on each impl PR
  BEFORE and AFTER; dependency-checker on the `trading/Cargo.toml` tokio-tungstenite
  new-consumer.
- **STAGE 0 (this):** `bash .claude/hooks/plan-gate.sh "$PWD"` PASS; the runbook
  cross-ref is satisfied structurally (variants land WITH the runbook in stage A).

## Rollback

- Config flip: `[groww_orders] order_push_enabled = false` (the serde default) —
  a running push channel stops connecting on the next boot; byte-identical to
  today. No table, no schema, no market-data path is touched by the channel, so
  disabling loses only the live order/position event stream (the poller remains
  the safety record).
- Build rollback: dropping the `groww_orders` feature from a build removes the
  entire push subtree at compile time.
- Git rollback: each serial PR is independently revertible; the transport restore
  (stage B) is additive under a feature gate. The order-audit consumer reuses the
  EXISTING `OrderAuditWriter` — no storage/table migration to unwind.
- The 4 GROWW-PUSH codes are log-sink-only — no alarm/terraform to remove on
  rollback.

## Observability

- **Codes (log-sink-only):** GROWW-PUSH-01/02/03/04 (runbook
  `groww-order-push-error-codes.md`); GROWW-ORD-08 (order-audit write, existing).
- **Counters (static labels):** `tv_groww_push_reconnects_total{stage}`,
  `tv_groww_push_auth_failures_total{auth_class}`,
  `tv_groww_push_decode_errors_total{kind}`,
  `tv_groww_push_supervisor_respawn_total{reason}`,
  `tv_groww_order_updates_total{status}`, `tv_groww_position_updates_total`.
- **Audit rows:** `ws_event_audit` (edge-latched Connected/Disconnected/Reconnected,
  `WsType::GrowwOrderUpdate`, feed=`groww`, via the EXISTING writer); `order_audit`
  (feed=`groww`, mode=`paper`, via the EXISTING `OrderAuditWriter` — feed is
  already in `DEDUP_KEY_ORDER_AUDIT`).
- **Delivery boundary:** no new Telegram, no `events.rs` edit, no
  `error-code-alarms.tf` entry, no `observability-architecture.md` paging-list
  mention (the paging drift guard sees no drift). CloudWatch alarms are a flagged
  follow-up.
- **Tracing:** each stage emits `error!` with `code = ErrorCode::X.code_str()` per
  the tag-guard; the runner logs a bounded reconnect/decode line.

---

## Guarantee matrices (per `.claude/rules/project/per-wave-guarantee-matrix.md`)

**15-row 100% matrix** — this STAGE 0 is docs-only; per-row proof lands in the
serial impl PRs (A–D) named above. Cross-reference:
`.claude/rules/project/per-wave-guarantee-matrix.md` (15 rows) +
`.claude/rules/project/operator-charter-forever.md` §C. Each impl PR fills the
9-box + coverage delta + DHAT/Criterion (transport is cold-path, not per-tick) +
crossref/lattice ratchets + adversarial 3-agent (hot-path + security + hostile).

**7-row resilience matrix** — cross-reference
`.claude/rules/project/per-wave-guarantee-matrix.md` (7 rows): no new tick-drop
path (this channel is order/position, not ticks); supervised-respawn preserves
the channel (WS-GAP-05 pattern); no hot-path allocation (cold-path connect/decode);
QuestDB reuse is the existing `OrderAuditWriter` self-heal; composite uniqueness —
`order_audit` DEDUP already carries `feed`; real-time proof — the 6 counters +
ws_event_audit rows + GROWW-PUSH codes.

**Honest 100% claim (mandated wording):** "100% inside the tested envelope, with
ratcheted regression coverage: the push channel is DEFAULT-OFF (config +
feature); a decode/schema drift is LOUD (GROWW-PUSH-03), never silent; the runner
is supervised (GROWW-PUSH-04). NOT claimed: LIVE Groww order/position push
behaviour (wire shape UNVERIFIED-LIVE — NEW proto DTOs + subjects, first enabled
session is the probe), order-push ENTITLEMENT (UNPROBED), or any order placement
(the channel is receive-only; live order-fire stays behind
`GROWW_ORDER_LIVE_FIRE = false`). Release-build panics abort the process
(`panic = "abort"`); respawn self-heal is unwind-build only."

## Plan Items (impl stages — checked as each serial PR merges)

- [ ] Stage A — 4 `GrowwPush0*` ErrorCode variants + `order_push_enabled` config +
  `WsType::GrowwOrderUpdate` + lattice-guard Gate-1 extension + this runbook cross-ref
  - Files: `crates/common/src/error_code.rs`, `crates/common/src/config.rs`,
    `config/base.toml`, `crates/common/src/ws_event_types.rs`,
    `crates/common/tests/groww_order_lattice_guard.rs`
  - Tests: error_code_rule_file_crossref, groww_order_lattice_guard, ws_type as_str
- [ ] Stage B — transport under `oms/groww/push/` (restore from `dd7eaa5^`) +
  `tokio-tungstenite` new-consumer
  - Files: `crates/trading/src/oms/groww/push/{socket_token,nkey,nats,proto,subjects,connect}.rs`,
    `crates/trading/Cargo.toml`
  - Tests: proto decode golden, nkey nonce-sign, subject builders; dependency-checker
- [ ] Stage C — order mapper + position handler + supervised runner + multi-sink fan-out
  - Files: `crates/trading/src/oms/groww/push/{order_mapper,position,runner}.rs`,
    `crates/trading/src/oms/groww/events.rs`
  - Tests: OrderDetailUpdateDto→BrokerOrderEvent mapper, respawn classify
- [x] Stage D — app wiring (order-audit consumer + main.rs spawn + app feature)
  - Files: `crates/app/src/groww_order_observability.rs`, `crates/app/src/main.rs`,
    `crates/app/src/lib.rs`, `crates/app/Cargo.toml`
  - Tests: sink try_send drop (GROWW-ORD-08), OrderAuditRow mapping
  - Implementation note: the app-side bounded-queue `sink_drop` carries
    `GROWW-PUSH-03` (`GrowwPush03DecodeFailed`, `stage = "sink_drop"`) per the
    Stage D contract documented on `GrowwPushEventSink` in
    `oms/groww/events.rs` (the runbook assigns no dedicated code for the
    app-side queue drop); consumer PERSIST failures carry `GROWW-ORD-08`
    (`GrowwOrd08AuditWriteFailed`, `stage = "append"`/`"flush"` — the
    groww-orders runbook §8 assignment); consumer task death respawns under
    `GROWW-PUSH-04` (`component = "audit_consumer"`).

## Scenarios

| # | Scenario | Expected |
|---|---|---|
| 1 | Default build (`order_push_enabled=false`) | no socket, byte-identical to today |
| 2 | Enabled, healthy session | order/position events decoded, order events → BrokerOrderEvent → hint sink + order_audit (feed=groww, mode=paper) |
| 3 | ~06:00 IST token reset | GROWW-PUSH-02 → SSM re-read (no mint) → reconnect, self-heals |
| 4 | Malformed proto frame | GROWW-PUSH-03 skip + count, channel continues |
| 5 | Runner panic (unwind build) | GROWW-PUSH-04 respawn + counter; release build aborts → restart |
| 6 | Order-audit write fails | GROWW-ORD-08 (append/flush/sink_drop), never gates anything |
