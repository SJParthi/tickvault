---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/trading/src/oms/groww/smart_orders.rs"
  - "crates/trading/src/oms/groww/executor.rs"
  - "crates/trading/src/oms/groww/api_client.rs"
  - "crates/trading/tests/groww_smart_orders_off_guard.rs"
  - "crates/common/src/config.rs"
---

# Groww Smart Orders (GTT/OCO) Area — Error Codes (GROWW-OCO-01..05)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39 +
> `no-rest-except-live-feed-2026-06-27.md` §10 (the 4-gate live-fire
> lattice the Groww order-side lives behind) > this file.
> **Operator context:** the 2026-07-14 Groww order-side build authorization
> (§39.0 verbatim: *"confirm — apply the Groww order scope-unlock PR-0.
> Build only, behind the OFF switch, no live orders"*) + the 2026-07-15
> coordinator fan-out mandate (contracts-first stubs) + the **2026-07-16
> operator implement-everything directive (verbatim: "why stubs gaps — i
> clearly told you to implement and integrate everything.")** under which
> the stubs below were converted to LIVE code.
> **Companion code (LIVE since 2026-07-16):**
> `crates/trading/src/oms/groww/smart_orders.rs` (types, wire serde,
> validation, per-type modify matrix, open-set status FSM, sibling-verify +
> reconcile pure logic, the spawnable reconcile loop, the 5 coded emit
> helpers), `crates/trading/src/oms/groww/api_client.rs` (the
> `SmartOrderTransport` trait + the `/v1/order-advance/*` paths — Gate 5
> confinement), `crates/trading/src/oms/groww/executor.rs` (the gated
> place/modify/cancel/reconcile driver + write-ahead SmartPlace/SmartModify/
> SmartCancel intents), `crates/trading/src/oms/groww/intent_ledger.rs`
> (the smart intent kinds join the open-place dedup index). All under the
> non-default `groww_orders` cargo feature (Gate 2).
> **Config knobs:** `GrowwOrdersConfig::{smart_orders_read,
> smart_orders_write, oco_reconcile_poll_secs, oco_sibling_cancel_deadline_secs}`
> — gate bools default OFF; cadences default 15/30.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `GrowwOco0*` variant verbatim —
> `GrowwOco01PlacementFailed`, `GrowwOco02SiblingCancelUnverified`,
> `GrowwOco03ReconcileMismatch`, `GrowwOco04ModifyRejected`,
> `GrowwOco05PollerDegraded` and `GROWW-OCO-01`, `GROWW-OCO-02`,
> `GROWW-OCO-03`, `GROWW-OCO-04`, `GROWW-OCO-05` appear below.

---

## §0. Status: IMPLEMENTED 2026-07-16 (supersedes the contract-stub state)

> **⚠ The 2026-07-15 "contracts-first stubs — ZERO production code emits
> these codes" state is SUPERSEDED 2026-07-16** per the operator directive
> quoted in the header. Every code below now has REAL emit sites in the
> area files; the sequencing caveats of the stub revision (OCO-02/OCO-03
> "emit sites land with a LATER orchestrator PR") are CLOSED — the
> sibling-verify and reconcile emit sites live in `smart_orders.rs`
> (`reconcile_pass` / the deadline sweep) and are driven by the executor's
> `reconcile_smart_orders` + the spawnable `run_smart_order_reconcile_loop`.
> What remains DELIBERATELY unwired: no boot orchestrator spawns the loop
> and no strategy path calls the surface — the no-production-caller ratchet
> `crates/trading/tests/groww_smart_orders_off_guard.rs` pins ZERO callers
> outside `oms/groww/` until a future wiring PR updates it deliberately.
> Every mutation stays behind the §39.2 4-gate lattice
> (`GROWW_ORDER_LIVE_FIRE = false`; `smart_orders_write` default OFF;
> a config flip alone yields PAPER mode only).

## §1. GROWW-OCO-01 — smart-order mutation failed

**Severity:** High. **Auto-triage safe:** Yes (the mutation already failed
loudly with its reference/id forensics; auto-triage must never re-issue an
order-side mutation).

**Trigger:** `ErrorCode::GrowwOco01PlacementFailed` — a smart-order
(GTT/OCO) CREATE or CANCEL leg failed or went unresolved. Emit helper:
`smart_orders::emit_oco01(leg, stage, id, detail)`; executor call sites:

| leg | stage | Meaning |
|---|---|---|
| `create` | `rejected` | definitive broker refusal (400-class + well-shaped GA envelope; the GA code + bounded message ride the payload) |
| `create` | `unresolvable_ambiguity` | timeout / 5xx / 429 / auth-stale / 2xx-without-id on a smart CREATE — **fail-closed**: the smart surface has NO status-by-reference lookup, so an ambiguous create is parked `Unresolved` and NEVER auto-replayed (the double-send class); operator reconciles by `reference_id` (`TV…`) against the broker book |
| `cancel` | `rejected` / `ambiguous` | broker refusal / ambiguous outcome on a cancel (path-params-only wire call, no body) |

Dry-run (paper) and config-off outcomes are NOT errors — they never emit
this code; only a real broker/transport failure on an authorized leg does.
Counter: `tv_groww_oco_mutations_total{leg, outcome}` (`leg` ∈
create/modify/cancel; `outcome` ∈ ok/rejected/unresolved/paper).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-01`; the payload
   carries `leg`, `stage`, the smart-order id (when known) and the bounded
   secret-redacted broker reason.
2. `tv_groww_oco_mutations_total{leg, outcome}` rates name the dominant
   class; cross-check the shared token + the Groww order-side REST health
   (GROWW-ORD-10 auth-stale semantics apply — token re-READ, never minted).
3. Never re-issue the mutation blind — the write-ahead intent (SmartPlace /
   SmartCancel in `data/orders/groww-intents-<date>.ndjson`) + the
   reconcile poller own the follow-up. An `unresolvable_ambiguity` create
   is checked in the Groww app by the `TV…` reference before ANY manual
   re-place.

## §2. GROWW-OCO-02 — OCO sibling-leg cancel UNVERIFIED (double-fill exposure)

**Severity:** **Critical.** **Auto-triage safe:** No — automatic via the
Critical blanket (`is_auto_triage_safe()` never returns true for
Critical; deliberately NOT an override-list entry —
`test_critical_codes_never_auto_triage` covers it).

**Trigger:** `ErrorCode::GrowwOco02SiblingCancelUnverified` — one OCO leg
reached a terminal FILLED state (Triggered observed) and the smart order
OBJECT was NOT observed cancelled/closed within
`oco_sibling_cancel_deadline_secs` (30; strict `>`, saturating
arithmetic). This is the never-assume-auto-cancel check: whether the
broker auto-cancels the sibling is NOT trusted — an unverified sibling is
a live order that can ALSO fill (a double-fill exposure window), which is
why this is Critical and operator-paged, never auto-actioned.

**Object-level honesty (live-probe P5):** the Groww smart-order surface
exposes NO per-leg order ids — sibling verification is at the SMART-ORDER
OBJECT level (the tracked order's observed status), not per-leg. Whether a
per-leg view exists on the live wire is UNVERIFIED-LIVE. Emit sites (both
in `smart_orders.rs::reconcile_pass`): the per-order deadline check
(`stage = "deadline_exceeded"`) and the unreadable-GET deadline sweep (a
GET that cannot be read past the deadline still pages — an unreadable
sibling is not a verified sibling). ONCE per episode per order
(`sibling_unverified_paged` latch on the tracked order); also counted as
`tv_groww_oco_reconcile_findings_total{kind="sibling_unverified"}`.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-02`; the payload
   names the smart-order id, the elapsed ms and the deadline.
2. Verify the order on the broker book IMMEDIATELY (get/list); if the
   sibling leg is still live, the operator cancels it manually — before it
   fills.
3. Post-incident: check the reconcile poller health (GROWW-OCO-05) — a
   degraded poller lengthens the verification window.

**Honest envelope additions (2026-07-16, adversarial round 1 — plain
statements, never softened):**
- **Restart blind window (finding 5):** the tracked smart-order book is
  process-RAM — a restart mid-OCO-episode EMPTIES it, and OCO-02 CANNOT
  fire for pre-restart orders until book rehydration lands. What IS
  persisted since 2026-07-16: every SmartPlace/SmartModify/SmartCancel
  intent line carries a `smart_ctx` record (segment + smart_order_type +
  trading_symbol + quantity, folded into the replay summary) — the full
  status-GET path is rebuildable from the ledger, so the future wiring
  PR's rehydration is a replay-seam read, not a schema change. Until it
  ships, a restart during a triggered-but-unverified OCO episode leaves
  the double-fill exposure UNPAGED — the operator's mitigation is the
  broker app check after any in-session restart with live OCOs.
- **Session-tail blind window (finding 13):** the reconcile loop runs
  in-market only — an OCO that TRIGGERS within
  `oco_sibling_cancel_deadline_secs` (30s) of the 15:30 close may never
  reach its sibling-verify deadline before the box stops; the episode
  resumes only if the order is still tracked (same process) next session.
- **Elapsed undercount (finding 13):** the episode clock arms at the
  OBSERVATION of TRIGGERED (poll time), not the broker-side trigger
  instant — the reported elapsed undercounts by up to one poll interval
  (`oco_reconcile_poll_secs`, 15s).

## §3. GROWW-OCO-03 — smart-order reconcile mismatch

**Severity:** High. **Auto-triage safe:** **No — severity-independent
override arm** in `is_auto_triage_safe()` (the FUTIDX-02 precedent): a
data-comparability signal between the broker book and our tracked state is
never auto-actioned; the operator decides which side is wrong.

**Trigger:** `ErrorCode::GrowwOco03ReconcileMismatch` — a reconcile pass
found ≥1 finding. ONE coalesced `error!` per pass (`stage =
"reconcile_pass"`, ≤5 sample findings on the payload — audit Rule 4, never
per-finding spam); every finding is ALSO counted per-kind on
`tv_groww_oco_reconcile_findings_total{kind}`:

| kind | Meaning |
|---|---|
| `status_regression` | an observed status is an illegal backward / out-of-terminal move (the rank-monotone open-set FSM parked it) |
| `quantity_drift` | observed quantity differs from the tracked quantity — INCLUDING the partial-fill class; broker-side partial-fill semantics are doc-UNKNOWN, so the check fails CLOSED (reported, never silently normalized) |
| `ghost_local` | a tracked non-terminal order is absent from the broker on 2 CONSECUTIVE sweeps (one missing sweep is list-lag grace, never a finding) |
| `qty_exceeds_position` | tracked OCO quantity exceeds \|net position\| (only when the caller supplies a position map — the future wiring PR's portfolio snapshot) |
| `unknown_status` | the broker served a status outside the documented vocabulary — the order is PARKED, raw string preserved on the finding |

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-03`; the payload
   carries the finding count + samples (id, kind, detail).
2. Do NOT hand-patch either side; the operator judges partial-fill vs
   broker-lag vs a tracked-state bug, and the next poll re-measures.
3. A `ghost_local` cluster right after a process restart usually means the
   day's ledger replay adopted intents the broker already expired —
   cross-check the intent ledger for the ids.

## §4. GROWW-OCO-04 — smart-order modify rejected

**Severity:** Medium. **Auto-triage safe:** Yes (the modify already
failed loudly; the caller's fallback policy owns the follow-up).

**Trigger:** `ErrorCode::GrowwOco04ModifyRejected` — a smart-order modify
was refused. Emit helper: `smart_orders::emit_oco04(stage, id, detail)`;
stages: `immutable_field` (typed refusal BEFORE any HTTP), `rejected`
(broker GA refusal), `ambiguous` (unresolved transport outcome — parked,
never auto-replayed).

**2026-07-16 doc correction (DOCS win — supersedes the stub's blanket
claim):** the stub asserted "leg `order_type` / `price` are API-immutable
on the Groww smart-order surface". That is the OCO half only. The
implemented per-type modifiable-field matrix
(`smart_orders::validate_modify_fields`, totality-tested):

| Type | MODIFIABLE | IMMUTABLE (→ `immutable_field` refusal) |
|---|---|---|
| GTT | `quantity`, `trigger_price`, the order LEG (`order_type` + price per the leg-price-shape rule) | `duration`, `product_type`, `target.trigger_price`, `stop_loss.trigger_price` (OCO-only fields) |
| OCO | `quantity`, `target.trigger_price`, `stop_loss.trigger_price` | `trigger_price`, `trigger_direction`, the order LEG (`order_type`/price — "(not modifiable)"), `child_legs` |

The fallback for a genuinely immutable field remains cancel + recreate —
a CALLER decision, never automatic.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-04`; the payload
   names the stage, the smart-order id and (for `immutable_field`) the
   targeted field + type.
2. An `immutable_field` rejection is the CALLER's request-construction
   bug — route through cancel+recreate, never relax the matrix.
3. `ambiguous` modifies are parked with a SmartModify `Unresolved` intent —
   the next reconcile poll re-observes the order's real state.

## §5. GROWW-OCO-05 — OCO reconcile poller degraded

**Severity:** High. **Auto-triage safe:** Yes (bounded re-poll at the
next tick).

**Trigger:** `ErrorCode::GrowwOco05PollerDegraded` — a reconcile
poll leg degraded. Emit helper: `smart_orders::emit_oco05(stage, detail)`;
counter `tv_groww_oco_poller_errors_total{stage}`:

| stage | Meaning |
|---|---|
| `token` | no access token available this turn, or the broker classified the read auth-stale (401/403) — the token is re-READ from SSM by the shared provider at its own pacing, NEVER minted (token-minter lock 2026-07-02) |
| `rate_limited` | HTTP 429 on a smart-order read — counted, skipped to the next poll tick, NEVER out-polled |
| `rejected_read` | the broker GA-refused a get/list read (the GA code rides the detail) |
| `transport` | timeout / 5xx / decode failure on a get/list read |

The loop is `smart_orders::run_smart_order_reconcile_loop` (cadence
`oco_reconcile_poll_secs` = 15; read-gated on `smart_orders_read`;
paper mode = zero-HTTP no-op). Gates are captured BY VALUE at spawn time
(finding 9) — a runtime gate flip needs a loop respawn; only the
market-hours closure and the token are re-read per turn. **Honest
boundary:** the loop is spawnable
but NOT yet spawned by any boot path (the §0 no-production-caller
ratchet); supervised-respawn wiring is the future wiring PR's duty — until
then a dead loop is impossible because no loop runs.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-05`; the payload
   names the stage + bounded detail.
2. A sustained rate WHILE an OCO pair is live is the dangerous shape —
   it blinds the GROWW-OCO-02 sibling verification; cross-check §2.
3. Sustained `token` outside the ~06:00 IST daily reset = the shared
   minter Lambda is stale — see `groww-shared-token-minter-2026-07-02.md`;
   never mint from here.

## §6. Honest envelope + delivery boundary

All five codes are **log-sink-only**: NO `error_code_alerts` map entry, no
CloudWatch filter, no Telegram emit — the typed Telegram surface lands
with the wiring PR (`crates/core/src/notification/events.rs` is a
build-lead shared file, untouched by the area implementation), and any
Groww page class is governed by the §39 live-fire lattice (any Dhan-side
page class by `dhan-rest-only-noise-lock-2026-07-14.md`). The coded
`error!` lines + counters are the operator surface today.

NOT claimed: live Groww smart-order behaviour — status vocabulary (the
implemented open-set is Active/Triggered/Cancelled/Completed/Expired/
Rejected + Unknown-parked), auto-cancel semantics, per-leg visibility
(P5), and the GA007 idempotency window are all UNVERIFIED-LIVE until the
first authorized session. **CASH-OCO contradiction (live-probe P9):** the
doc arms agree a CASH-segment OCO is MIS-only (the implementation refuses
`CashOcoProductNotMis` accordingly) but whether CASH OCO is SERVED at all
on the live wire is contradicted across doc surfaces — Unknown until
probed. Write paths have NO transport retry (the double-send class); the
smart CREATE additionally has NO reference-status lookup, so ambiguity is
fail-closed-parked (§1), never resolved by resend. Wire prices are
DECIMAL STRINGS on the smart surface (regular orders use JSON numbers) —
integer paise at rest, sub-paise refused, ±1e12-paise band.
Transport hardening (adversarial round 1, 2026-07-16 — code-verified, the
claims below are RATCHETED by
`api_client.rs::ratchet_client_builder_has_no_redirects_and_oversize_caps`):
the shared order-surface reqwest client pins redirect `Policy::none`;
`send_and_classify` refuses a response on the DECLARED Content-Length
pre-read AND post-read past `ORDER_MAX_RESPONSE_BYTES` (2 MiB — the
margin.rs oversize pattern); smart-order ids are plausibility-checked IN
THE TRANSPORT itself before any URL splice (modify/cancel/get — an
implausible id is a typed never-sent refusal, `refuse_implausible_smart_id`)
in addition to the executor-side checks; a broker-ACK id must ALSO pass
plausibility before entering the tracked book (an implausible echo is an
unresolved place, `tv_groww_oco_implausible_ack_id_total`); log lines
route ids through the `log_safe_id` sanitize choke point. No order
fire is possible: Gates 1–4 all closed (`dry_run` stays true; paper mode
returns deterministic `PAPER-<reference_id>` ids with zero HTTP — ratcheted by
`groww_orders_transport_guard.rs`'s gate-6 scan, which now includes
`smart_orders.rs`).

## §7. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwOco0*` variant)
- `crates/trading/src/oms/groww/smart_orders.rs`
- `crates/trading/src/oms/groww/executor.rs` (the smart driver block)
- `crates/trading/src/oms/groww/api_client.rs` (`SmartOrderTransport`)
- `crates/trading/tests/groww_smart_orders_off_guard.rs`
- `crates/common/src/config.rs` (`GrowwOrdersConfig` smart-order fields)
- Any file containing `GROWW-OCO-01`, `GROWW-OCO-02`, `GROWW-OCO-03`,
  `GROWW-OCO-04`, `GROWW-OCO-05`, `GrowwOco01PlacementFailed`,
  `GrowwOco02SiblingCancelUnverified`, `GrowwOco03ReconcileMismatch`,
  `GrowwOco04ModifyRejected`, `GrowwOco05PollerDegraded`,
  `SmartOrderTransport`, `run_smart_order_reconcile_loop`,
  `oco_reconcile_poll_secs`, `oco_sibling_cancel_deadline_secs`, or
  `tv_groww_oco_`
