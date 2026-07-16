# Implementation Plan: Groww Smart Orders (GTT/OCO) — full implementation behind the 4-gate lattice

**Status:** APPROVED
**Date:** 2026-07-16
**Approved by:** Parthiban (operator) — dated directive 2026-07-16: "why stubs gaps — i clearly told you to implement and integrate everything."

> Scope authority: `groww-second-feed-scope-2026-06-19.md` §39 +
> `no-rest-except-live-feed-2026-06-27.md` §10 (the 4-gate live-fire lattice) +
> `.claude/rules/project/groww-oco-error-codes.md` (the GROWW-OCO-01..05
> contract stubs this plan converts to live emit sites). Docs ground truth:
> `docs/groww-ref/18-smart-orders-schemas.md` (Verified-capture) +
> `docs/groww-ref/16-orders-margins-portfolio.md` §5 (GA envelope).
> Crates touched: **tickvault-trading** (`crates/trading/src/oms/groww/`) only
> — plus the rule file + guard tests. NO crates/app wiring, NO
> `crates/core/src/notification/events.rs` edit, NO new dependency.

## Design

The Smart Orders area lands as `crates/trading/src/oms/groww/smart_orders.rs`
(the §39.3 area file, feature `groww_orders`), integrated through the EXISTING
Orders-area machinery rather than a parallel stack:

- **Transport (Gate 5 confinement):** the `/v1/order-advance/*` endpoint PATH
  strings + all HTTP live ONLY in `crates/trading/src/oms/groww/api_client.rs`
  — a new `SmartOrderTransport` trait (native async-fn-in-trait, no dyn)
  beside `OrderTransport`, implemented by `GrowwOrderApiClient` (reqwest;
  same classify_response / GrowwEnvelope tolerant parse / body-excerpt
  sanitize / timeouts) and by `NullTransport` (inert — the paper lane's
  zero-HTTP transport). Endpoints per the docs: create
  `POST /v1/order-advance/create`; modify
  `PUT /v1/order-advance/modify/{id}` (202 partial echo); cancel
  `POST /v1/order-advance/cancel/{segment}/{type}/{id}` (202, NO body); get
  `GET /v1/order-advance/status/{segment}/{type}/internal/{id}`; list
  `GET /v1/order-advance/list` (explicit type+status params — the OCO/ACTIVE
  defaults bite; page_size clamped 1..=50, page ≤ 500).
- **Types + validation (`smart_orders.rs`):** GTT + OCO create request
  structs field-for-field from `18-smart-orders-schemas.md` (§2.2/§2.3);
  prices are DECIMAL STRINGS on this wire — integer paise internally with
  pure `paise_to_decimal_string` / `decimal_string_to_paise` (checked
  integer math, ±1e12-paise band, no float in the conversion); `child_legs`
  stays an opaque `Option<serde_json::Value>` (structure undocumented —
  probe P1). OCO is exit-only: `net_position_quantity` required, quantity ≤
  abs(net), net ≠ 0; CASH-OCO with a non-MIS product is refused (BOTH doc
  arms agree on MIS-only; availability itself is the contradicted P9 probe).
  NO invented leg-price relative ordering (the docs state none).
- **Per-type modify matrix (the doc-corrected rule):** GTT modifiable =
  quantity, trigger_price, trigger_direction, the whole `order` leg
  (order_type + price shape + required-but-not-modifiable transaction_type),
  child_legs; GTT immutable = duration, product_type. OCO modifiable =
  quantity, duration, product_type, target.trigger_price,
  stop_loss.trigger_price; OCO immutable = leg order_type/price. A modify
  carrying an immutable field for its type is a TYPED refusal BEFORE any
  HTTP → GROWW-OCO-04.
- **Status FSM:** 6-value open-set `SmartOrderStatus` (ACTIVE/TRIGGERED/
  CANCELLED/EXPIRED/FAILED/COMPLETED + Unknown(raw)) — rank-monotone
  transitions (ACTIVE→TRIGGERED→terminal), backward/out-of-terminal → Park,
  Unknown → Park, total no-panic parse.
- **Sibling-verify (GROWW-OCO-02):** pure `evaluate_sibling_verify` over
  (triggered_at, now, deadline, latest status). Honest envelope: leg-level
  ids are undocumented (P5), so verification is at the smart-order-OBJECT
  level — a settled terminal within `oco_sibling_cancel_deadline_secs` (30)
  = verified; TRIGGERED (or unreadable) past the deadline = UNVERIFIED →
  one Critical GROWW-OCO-02 per episode (latched).
- **Reconcile:** pure classification (status regression / quantity drift /
  ghost_local after 2 confirm sweeps / qty-exceeds-position when a position
  map is supplied) + `reconcile_pass` (one bounded pass: per-tracked GET +
  per-type ACTIVE list for foreign counting) + a spawnable
  `run_smart_order_reconcile_loop` (margin-loop pattern: interval =
  `oco_reconcile_poll_secs`, gates re-read per turn, shutdown watch). NO
  crates/app spawn site in this PR.
- **Executor integration (`executor.rs`):** `GrowwOrderExecutor` gains a
  shared `Arc<tokio::sync::Mutex<SmartOrderBook>>` + engine entry points
  `place_smart_order` / `modify_smart_order` / `cancel_smart_order` /
  `reconcile_smart_orders` in an `impl<T: OrderTransport +
  SmartOrderTransport>` block — the same object the regular-order flow uses.
  Every mutation is write-ahead: `IntentKind::{SmartPlace,SmartModify,
  SmartCancel}` (additive `intent_ledger.rs` variants; smart places join the
  duplicate-reference index via an `is_place_class` helper) fsynced BEFORE
  any send; mutation refused on append failure. Paper mode = deterministic
  simulated ACTIVE smart order (`PAPER-<ref>` ids, zero HTTP). Live mode
  re-checks `smart_live_send_permitted` = `GROWW_ORDER_LIVE_FIRE &&
  live_fire_requested && smart_orders_write` INSIDE the send fn
  (defense-in-depth over Gates 2/3); write paths have NO transport retry
  (double-send class — GA007 window unproven, P10).
- **Gates struct:** `SmartOrderGates::from_config(&GrowwOrdersConfig)`
  carries smart_orders_read/write + live_fire_requested + max qty + the two
  OCO cadences — `ExecutorConfig` is untouched.

## Edge Cases

- Decimal-string money: zero, negative (refused at validation; parser is
  total), >2 fraction digits (accepted only when the extra digits are all
  zero), missing/empty/garbage strings, i64 overflow (checked math → None),
  the exact ±1e12-paise band edges.
- OCO quantity vs position: qty == abs(net) (allowed), qty > abs(net)
  (refused), net == 0 (refused — exit-only), negative net (short position —
  abs() applied).
- Modify: empty field set (refused); immutable field per type (refused,
  OCO-04); GTT order-leg price shape per order_type (LIMIT/SL require price,
  MARKET/SL_M require null); modification count past
  `GROWW_ORDER_MAX_MODIFICATIONS_PER_ORDER` (refused).
- Status parse: unknown strings park (raw preserved), case/whitespace
  tolerated, empty string → Unknown, control chars never panic.
- FSM totality: every (current × observed) pair over the 7-arm enum lands on
  exactly one of Transition / SameStatusRefresh / Park.
- Sibling verify: trigger observed exactly at the deadline (strictly-past
  fires), clock going backwards (saturating), episode re-latch only after a
  settled terminal, GTT TRIGGERED never arms the OCO-02 machinery.
- List responses: missing `orders` array, rows missing every field, foreign
  (untracked) rows counted but NEVER acted on (no reference_id exists on
  smart-order responses — adoption impossible, verified doc absence).
- Tracked-book cap (`GROWW_ORDER_MAX_TRACKED_OPEN_ORDERS`) — new smart
  places refused at the cap.

## Failure Modes

- Broker definitive reject (400 + well-shaped GA envelope) → GROWW-OCO-01
  (create/cancel) / GROWW-OCO-04 (modify), counted, never retried.
- Ambiguous create (timeout/5xx/429/decode/2xx-missing-id): smart-order
  responses carry NO reference_id, so the regular-order by-reference
  resolution ladder is IMPOSSIBLE — the intent is journaled Ambiguous →
  Unresolved, GROWW-OCO-01 fires (stage names the arm), NO auto-retry
  (double-send class). Operator + the reconcile poller own the follow-up.
- Ambiguous modify/cancel: the smart_order_id is known → the reconcile
  poller confirms actual state; coded OCO-04/OCO-01 stage lines fire.
- Ledger append failure → mutation REFUSED (LedgerUnavailable — write-ahead
  discipline unchanged).
- Poller legs: AuthStale (token reset class) aborts the pass (stage
  `token`), 429 stops the pass (never out-polled), transport/decode degrade
  single reads — all GROWW-OCO-05, coalesced once per pass.
- Rate budget denial on live mutations → typed refusal before send.
- Paper lane can NEVER reach HTTP (NullTransport + Gate-6 scan extended to
  smart_orders.rs).

## Test Plan

In-module `#[cfg(test)]` in `crates/trading/src/oms/groww/smart_orders.rs`:
money conversion boundaries + proptest round-trip; wire-shape serde tests
(exact field names per the docs, GTT + OCO + modify partial legs + cancel
path building); validation boundaries (quantity zero/negative/cap/position,
price shapes, reference ids, CASH-OCO product); per-type modify matrix
totality (every field × both types); status open-set parse; FSM totality
table; sibling-verify decision fn (paused-clock style); reconcile
classification; list-query clamping. `api_client.rs` tests extend the
classification suite for the smart payloads. `executor.rs` tests (via
`executor_tests.rs` include) add paper-lane determinism + gate-refusal +
mock-transport live arms where cheap. New ratchet
`crates/trading/tests/groww_smart_orders_off_guard.rs` pins ZERO production
callers of the smart entry points outside `oms/groww/`;
`crates/trading/tests/groww_orders_transport_guard.rs` gate-6 scan EXTENDED
(never weakened) to cover `smart_orders.rs`. Suites: `cargo test -p
tickvault-trading --features groww_orders` AND the default no-feature build
both green; `cargo clippy --features groww_orders -- -D warnings -W
clippy::perf`; banned-pattern scanner.

## Rollback

Everything ships dark behind the unchanged 4-gate lattice: `[groww_orders]`
smart_orders_read/write default false (Gate 1), the non-default
`groww_orders` cargo feature (Gate 2 — a default build contains none of this
code), `GROWW_ORDER_LIVE_FIRE = false` (Gate 3), the rule lock (Gate 4). A
plain `git revert` of the implementation commits restores the stub state —
no config, schema, table, or deploy surface changes in this PR; the intent
ledger's new `smart_*` kinds are additive NDJSON labels (old ledgers replay
unchanged; a rolled-back binary would fail-loud on replaying a smart line,
acceptable since the lane is dark).

## Observability

Coded structured logs (log-sink-only per rule §6 — no Telegram, no
CloudWatch entry): GROWW-OCO-01 (create/cancel failed — stage taxonomy),
GROWW-OCO-02 (sibling unverified, Critical, once per episode), GROWW-OCO-03
(reconcile findings, coalesced per pass, ≤5 samples), GROWW-OCO-04 (modify
refused/rejected), GROWW-OCO-05 (poller degraded, stage taxonomy) — every
`error!` carries `code = ErrorCode::GrowwOco0X.code_str()` + `stage`
(tag-guard compliant); bounded sanitized excerpts only. Counters (static
labels): `tv_groww_oco_mutations_total{leg,outcome}`,
`tv_groww_oco_reconcile_findings_total{kind}`,
`tv_groww_oco_poller_errors_total{stage}`. The durable record is the
existing fsynced intent ledger (mode-tagged paper/live). An in-module
emit-count ratchet pins the live emit sites (the margin.rs precedent).

## Plan Items

- [x] Item 1 — Plan file (this document, own commit)
  - Files: .claude/plans/active-plan-groww-smart-orders.md
  - Tests: n/a (docs)
- [x] Item 2 — Smart-order area module: types, money, validation, modify
      matrix, status FSM, sibling verify, reconcile pure logic, gates,
      paper/live flow helpers, reconcile pass + loop
  - Files: crates/trading/src/oms/groww/smart_orders.rs,
    crates/trading/src/oms/groww/mod.rs
  - Tests: test_paise_decimal_string_boundaries, prop_decimal_string_roundtrip,
    test_create_gtt_serializes_exact_wire_names,
    test_create_oco_serializes_exact_wire_names,
    test_validate_oco_quantity_position_boundaries,
    test_modify_matrix_totality_per_type, test_smart_status_parse_open_set,
    test_smart_transition_totality, test_sibling_verify_deadline_edges,
    test_reconcile_classification, test_list_query_clamps
- [x] Item 3 — Transport extension (Gate-5 confined)
  - Files: crates/trading/src/oms/groww/api_client.rs
  - Tests: test_smart_cancel_path_shape, test_smart_status_path_shape,
    classification reuse tests
- [x] Item 4 — Ledger smart intent kinds (additive)
  - Files: crates/trading/src/oms/groww/intent_ledger.rs
  - Tests: test_smart_kinds_labels_and_place_class
- [x] Item 5 — Executor integration (entry points + book + gates)
  - Files: crates/trading/src/oms/groww/executor.rs,
    crates/trading/src/oms/groww/executor_tests.rs
  - Tests: test_smart_place_paper_deterministic,
    test_smart_write_gate_refused_live, test_smart_modify_immutable_refused
- [x] Item 6 — Guards: extend transport guard; new off-guard ratchet
  - Files: crates/trading/tests/groww_orders_transport_guard.rs,
    crates/trading/tests/groww_smart_orders_off_guard.rs
  - Tests: test_gate6_paper_lane_zero_http_import_scan (extended),
    test_smart_order_entry_points_have_no_production_callers
- [x] Item 7 — Rule file fleshed from stubs (doc corrections recorded)
  - Files: .claude/rules/project/groww-oco-error-codes.md
  - Tests: error_code_rule_file_crossref (existing, must stay green)

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` — the
15-row 100% matrix + the 7-row resilience matrix apply to every item above
(hot path untouched; no new tick-drop path; composite-key discipline N/A —
no QuestDB table is added; all gates ratcheted build-failing).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Paper GTT create | ledger smart_place recorded → PAPER-id ACTIVE, zero HTTP |
| 2 | Live create attempted today | refused — smart_live_send_permitted is false (Gate 3) |
| 3 | OCO modify carrying trigger_direction | typed refusal pre-HTTP, GROWW-OCO-04 |
| 4 | OCO TRIGGERED, no terminal within 30s | GROWW-OCO-02 Critical, once per episode |
| 5 | Reconcile sees CANCELLED→ACTIVE | Park + GROWW-OCO-03 finding, never normalized |
| 6 | Poller hits 401 | pass aborts, GROWW-OCO-05 stage=token, next tick retries |
