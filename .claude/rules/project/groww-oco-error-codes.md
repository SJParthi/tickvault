# Groww Smart Orders (GTT/OCO) Area — Error Codes (GROWW-OCO-01..05)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39 +
> `no-rest-except-live-feed-2026-06-27.md` §10 (the 4-gate live-fire
> lattice the Groww order-side lives behind) > this file.
> **Operator context:** the 2026-07-14 Groww order-side build authorization
> (§39.0 verbatim: *"confirm — apply the Groww order scope-unlock PR-0.
> Build only, behind the OFF switch, no live orders"*) + the 2026-07-15
> coordinator fan-out mandate (contracts-first stubs — relayed intent,
> labeled as such).
> **Companion code (FUTURE — lands in the Smart Orders area code PR):**
> `crates/trading/src/oms/groww/smart_orders.rs` (feature `groww_orders`,
> per §39.3). Config knobs land NOW with these stubs
> (`GrowwOrdersConfig::{smart_orders_read, smart_orders_write,
> oco_reconcile_poll_secs, oco_sibling_cancel_deadline_secs}` — all gate
> bools default OFF; the cadences default 15/30). NO production code
> emits these codes today.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `GrowwOco0*` variant verbatim —
> `GrowwOco01PlacementFailed`, `GrowwOco02SiblingCancelUnverified`,
> `GrowwOco03ReconcileMismatch`, `GrowwOco04ModifyRejected`,
> `GrowwOco05PollerDegraded` and `GROWW-OCO-01`, `GROWW-OCO-02`,
> `GROWW-OCO-03`, `GROWW-OCO-04`, `GROWW-OCO-05` appear below.

---

## §0. Why these codes exist (contracts-first stubs)

The Groww Smart Orders area (GTT/OCO create / modify / cancel + the
reconcile poller, §39.3) lands as its own area code PR on
`crates/trading/src/oms/groww/smart_orders.rs`. Per the INSTR-FETCH
Sub-PR #9 / GROWW-SCALE contracts-first sequencing, the five `ErrorCode`
variants + this runbook land FIRST so the area PR compiles against stable
identifiers with zero shared-file contention.

**Honest envelope (§F, stated plainly):** this PR ships ONLY contract
stubs — ZERO production code emits these codes until the area code PRs
land. Emit-site sequencing, stated per-code honestly: the GROWW-OCO-01
(create + cancel legs), GROWW-OCO-04 (modify) and GROWW-OCO-05 (get/list
poller) emit sites land with the `smart_orders.rs` area PR; the
GROWW-OCO-02 / GROWW-OCO-03 verdict TYPES exist in the area code but
their emit sites land with a LATER orchestrator PR. Every mutation stays
behind the §39.2 4-gate lattice (`GROWW_ORDER_LIVE_FIRE = false`); the
cadences below (`oco_reconcile_poll_secs = 15`,
`oco_sibling_cancel_deadline_secs = 30`) are the area PR's design values.

## §1. GROWW-OCO-01 — smart-order placement failed

**Severity:** High. **Auto-triage safe:** Yes (the placement already
failed loudly with its correlation forensics; auto-triage must never
re-issue an order-side mutation).

**Trigger:** `ErrorCode::GrowwOco01PlacementFailed` — a smart-order
(GTT/OCO) create or cancel leg failed after ONE bounded transport retry.
Dry-run (paper) and config-off outcomes are NOT errors — they never emit
this code; only a real broker/transport failure on an authorized leg does.
Emit sites land with the `smart_orders.rs` area PR.

**Triage (stub — counters land with the area PRs):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-01`; the payload
   carries the leg (create/cancel) + the bounded, secret-redacted broker
   reason.
2. `tv_groww_oco_mutations_total{leg, outcome}` (area-PR telemetry) rates
   name the dominant class; cross-check the shared token + the Groww
   order-side REST health.
3. Never re-issue the mutation blind — the idempotency machinery + the
   reconcile poller own the follow-up.

## §2. GROWW-OCO-02 — OCO sibling-leg cancel UNVERIFIED (double-fill exposure)

**Severity:** **Critical.** **Auto-triage safe:** No — automatic via the
Critical blanket (`is_auto_triage_safe()` never returns true for
Critical; deliberately NOT an override-list entry —
`test_critical_codes_never_auto_triage` covers it).

**Trigger:** `ErrorCode::GrowwOco02SiblingCancelUnverified` — one OCO leg
reached a terminal FILLED state and the sibling leg was NOT observed
cancelled within `oco_sibling_cancel_deadline_secs` (30). This is the
never-assume-auto-cancel check: whether the broker auto-cancels the
sibling is not trusted — an unverified sibling is a live order that can
ALSO fill (a double-fill exposure window), which is why this is Critical
and operator-paged, never auto-actioned. The verdict TYPE exists in the
area code; the emit site lands with the LATER orchestrator PR.

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-02`; the payload
   names the pair, the filled leg, and the elapsed unverified window.
2. Verify the sibling on the broker book IMMEDIATELY (get/list); if it is
   still live, the operator cancels it manually — before it fills.
3. Post-incident: check the reconcile poller health (GROWW-OCO-05) — a
   degraded poller lengthens the verification window.

## §3. GROWW-OCO-03 — OCO pair state reconcile mismatch

**Severity:** High. **Auto-triage safe:** **No — severity-independent
override arm** in `is_auto_triage_safe()` (the FUTIDX-02 precedent): a
data-comparability signal between the broker book and our tracked pair
state is never auto-actioned; the operator decides which side is wrong.

**Trigger:** `ErrorCode::GrowwOco03ReconcileMismatch` — the OCO pair
state reconcile found a mismatch vs the broker: OCO quantity vs
`abs(net position)` divergence, INCLUDING the partial-fill class. The
broker-side semantics here are doc-UNKNOWN — the check fails CLOSED
(a mismatch is reported, never silently normalized). The verdict TYPE
exists in the area code; the emit site lands with the LATER orchestrator
PR.

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-03`; the payload
   carries both sides' quantities + the pair identity.
2. Do NOT hand-patch either side; the operator judges partial-fill vs
   broker-lag vs a tracked-state bug, and the next poll re-measures.

## §4. GROWW-OCO-04 — smart-order modify rejected

**Severity:** Medium. **Auto-triage safe:** Yes (the modify already
failed loudly; the caller's fallback policy owns the follow-up).

**Trigger:** `ErrorCode::GrowwOco04ModifyRejected` — a smart-order modify
was rejected by the broker OR targeted a non-modifiable leg field (leg
`order_type` / `price` are API-immutable on the Groww smart-order
surface; the fallback path is cancel + recreate). Emit sites land with
the `smart_orders.rs` area PR.

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-04`; the payload
   names the targeted field + the broker reason.
2. An immutable-field rejection is the CALLER's request-construction bug
   — route through cancel+recreate, never relax the field check.

## §5. GROWW-OCO-05 — OCO reconcile poller degraded

**Severity:** High. **Auto-triage safe:** Yes (bounded re-poll at the
next tick; a supervisor respawn self-heals a dead poller).

**Trigger:** `ErrorCode::GrowwOco05PollerDegraded` — the get/list
reconcile poller (`oco_reconcile_poll_secs` = 15) degraded (transport /
parse / token failure on a poll tick) or was respawned. Bounded re-poll
happens at the next tick. Emit sites land with the `smart_orders.rs`
area PR.

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-OCO-05`; the payload
   names the failing leg / respawn reason.
2. A sustained rate WHILE an OCO pair is live is the dangerous shape —
   it blinds the GROWW-OCO-02 sibling verification; cross-check §2.

## §6. Honest envelope + delivery boundary

All five codes are **log-sink-only**: NO `error_code_alerts` map entry, no
CloudWatch filter, no Telegram emit — the typed Telegram surface lands
with the area code PRs, and any Groww page class is governed by the §39
live-fire lattice (any Dhan-side page class by
`dhan-rest-only-noise-lock-2026-07-14.md`). NOT claimed: live Groww
smart-order behaviour (status vocabulary, auto-cancel semantics,
immutable-field set — all UNVERIFIED-LIVE until the first authorized
session); any order fire (Gates 1–4 all closed; `dry_run` stays true).

## §7. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwOco0*` variant)
- `crates/trading/src/oms/groww/smart_orders.rs` (the future area file)
- `crates/common/src/config.rs` (`GrowwOrdersConfig` smart-order fields)
- Any file containing `GROWW-OCO-01`, `GROWW-OCO-02`, `GROWW-OCO-03`,
  `GROWW-OCO-04`, `GROWW-OCO-05`, `GrowwOco01PlacementFailed`,
  `GrowwOco02SiblingCancelUnverified`, `GrowwOco03ReconcileMismatch`,
  `GrowwOco04ModifyRejected`, `GrowwOco05PollerDegraded`, `smart_order`,
  `oco_reconcile_poll_secs`, or `oco_sibling_cancel_deadline_secs`
