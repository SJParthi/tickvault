# Groww Regular Orders — Error Codes (GROWW-ORD-01..10)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39 (the Groww order-side grant) >
> `no-rest-except-live-feed-2026-06-27.md` §10 (the sub-minute order-read
> cadence grant) > this file.
> **Scope:** REGULAR Groww orders — place (LIMIT/MARKET/SL/SL_M, DAY), modify,
> cancel, order-book/list, trade-book, status by id + by reference. GTT/OCO and
> the order-update push feed are separate sessions. DORMANT / dry-run only: no
> live Groww order can leave the box until the 4-gate live-fire lattice (§39.2)
> aligns behind a SEPARATE future dated operator quote.
> **Companion contracts:** `crates/common/src/error_code.rs` (the 10
> `GrowwOrd0*` / `GrowwOrd10AuthStale` variants), `crates/common/src/config.rs`
> (`GrowwOrdersConfig`), `crates/common/src/constants.rs` (`GROWW_ORDER_*`),
> `crates/core/src/notification/events.rs` (the 6 `GrowwOrder*` typed events),
> `crates/common/src/broker_order_events.rs` (the `BrokerOrderEvent` seam +
> `push_active`).
> **Companion code (lands in later serial PRs):**
> `crates/trading/src/oms/groww/{intent_ledger,state,reconcile,executor,api_client}.rs`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `GrowwOrd0*` variant's code_str verbatim —
> `GROWW-ORD-01` … `GROWW-ORD-10` all appear below.

---

## §0. Why these codes exist (the shared contract)

The Groww regular-order lane is a **parallel executor** under `oms/groww/`
(the frozen Dhan OMS is untouched). Its safety spine is: a fsynced write-ahead
**intent ledger** (mutation REFUSED on append failure), a broker
reference-id / GA007 idempotency loop, a rank-monotone open-set **state
machine** that is poll-skip-proof, and fail-closed self-capped rate budgets.
Every failure class along that spine is one of the ten codes below.

All ten ship **LOG-SINK-ONLY** — there is NO `error-code-alarms.tf` entry and
NO `observability-architecture.md` paging-list mention (the paging drift guard
pins both surfaces untouched). The operator page for an order incident is the
typed Telegram event (`GrowwOrderAmbiguous` / `GrowwOrderAmbiguityUnresolved`
/ `GrowwOrderRejected` / `GrowwOrderCancelLostRace` / `GrowwOrderReconcileMismatch`
/ `GrowwOrdersPaperDigest`); the coded `error!` lines are the forensic WHY.
CloudWatch promotion is a flagged follow-up (the `rest-1m-pipeline-error-codes.md`
§3 precedent).

---

## §1. GROWW-ORD-01 — mutation definitively rejected

**Severity:** High. **Auto-triage safe:** Yes.

**Trigger:** a place/modify/cancel mutation was DEFINITIVELY refused — a
400-class HTTP status AND a well-shaped FAILURE envelope
(`{"status":"FAILURE","error":{"code":"GAxxx",…}}`), or a REJECTED/FAILED order
status on a subsequent read. A definitive reject requires BOTH the 400-class
status and the well-shaped envelope; anything else on a mutation is AMBIGUOUS
(GROWW-ORD-02), NOT a clean refusal — this explicitly covers GA003 ("Unable to
serve request currently") and any GA code riding a non-400-class status.
GA-coded business rejects NEVER trip the circuit breaker.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — read the `GROWW-ORD-01` payload: the GA
   code + the request-specific plain `message`.
2. GA001 (bad request) / GA006 (cannot process) name a request the broker
   refused — fix the order shape; never auto-retried by design.
3. Cross-check the typed `GrowwOrderRejected` Telegram (coalesced count +
   sample reason).

## §2. GROWW-ORD-02 — ambiguous outcome (resolution ladder engaged)

**Severity:** High. **Auto-triage safe:** Yes.

**Trigger:** a mutation entered `phase=ambiguous` — timeout / 5xx / 504 / decode
failure / 429 / a GA code on a non-400-class status / a 2xx SUCCESS whose payload
carried no usable `groww_order_id`. The write-ahead intent enters the FULL
resolution ladder (poll `GET /v1/order/status/reference/{ref}` at 2s/5s/10s/30s/60s
then 60s-paced to the 600s bound — `GROWW_ORDER_AMBIGUITY_LADDER_STEPS_SECS` /
`GROWW_ORDER_AMBIGUITY_LADDER_MAX_SECS`) on the SAME `reference_id`. A new place
for the same logical order is refused (`DuplicateIntent`) until it resolves.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-02`; the payload names
   `op` + the `intent_id`.
2. No action while the ladder runs (it self-resolves or escalates to
   GROWW-ORD-03). The typed `GrowwOrderAmbiguous` Telegram fires once per
   episode.

## §3. GROWW-ORD-03 — ambiguity unresolved (ladder exhausted)

**Severity:** Critical. **Auto-triage safe:** No (Critical is never
auto-actioned).

**Trigger:** the resolution ladder EXHAUSTED its bounded budget
(`GROWW_ORDER_AMBIGUITY_LADDER_MAX_SECS` = 600s, with the 401/403 auth-stale
clock paused — see GROWW-ORD-10) without confirming an order's fate. A
token-recovery event re-arms ONE bounded automatic re-resolution pass before
demanding the operator.

**Triage:**
1. **Open the Groww app NOW** and check the order book — an order may be live
   and unknown to us. The typed `GrowwOrderAmbiguityUnresolved` Telegram carries
   the same instruction + `elapsed_secs`.
2. Reconcile the order against the local intent ledger
   (`data/orders/groww-intents-<IST-date>.ndjson`) by `reference_id`
   (`TV…` prefix).
3. Never auto-resend a modify/place on an unresolved intent (O-11 modify /
   O-10 uniqueness both Unknown-critical).

## §4. GROWW-ORD-04 — reconcile mismatch

**Severity:** High. **Auto-triage safe:** No — severity-INDEPENDENT override
(the FUTIDX-02 precedent): a data-integrity verdict is an operator judgment,
never auto-actioned despite High severity.

**Trigger:** the pure reconcile fn found a drift AFTER stale-snapshot-skip
filtering — a `status_drift` (broker truth via the §5 machine), a `fill_drift`
(integer-paise compare, never float epsilon), a `ghost_local` /
`ghost_broker` (our `TV…` prefix ⇒ adopt+audit; foreign reference ids = BruteX
co-tenant orders, counted + IGNORED, never touched), or a live-source
`filled_quantity` DECREASE (a fill-monotonicity breach). A backward observation
from the RECONCILE snapshot source classifies `stale_snapshot_skip` (counter,
debug) UNLESS repeated on 2 consecutive sweeps; a `ghost_local` needs the
min-age grace (older than one sweep interval AND absent on 2 consecutive
sweeps) before any Expired-class action.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-04`; the `kind` +
   `count` name the mismatch class.
2. Decide which side drifted (our records vs the broker); the typed
   `GrowwOrderReconcileMismatch` Telegram carries `kind`/`count`/`symbol`.

## §5. GROWW-ORD-05 — rate limited (co-tenant hypothesis)

**Severity:** Medium. **Auto-triage safe:** Yes.

**Trigger:** the broker returned HTTP 429 on an order-family call while our
self-caps (`GROWW_ORDER_MUTATIONS_PER_SECOND_CAP` 5/s ·
`GROWW_ORDER_MUTATIONS_PER_MINUTE_CAP` 100/min ·
`GROWW_ORDER_READS_PER_SECOND_CAP` 8/s ·
`GROWW_ORDER_READS_PER_MINUTE_CAP` 200/min) were green — the co-tenant (BruteX)
hypothesis, since the rate bucket is TYPE-LEVEL POOLED and shares the one minter
token (U-5, Unknown). Counted (`tv_groww_order_rate_limited_total{family}`) +
backed off, never out-polled, never trips the circuit breaker.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-05`; the `family`
   (orders / reads) + the sanitized ≤300-char body + any Retry-After.
2. A sustained rate while self-caps stay green = a co-tenant burst — coordinate
   the shared-account budget; never raise the self-caps to out-poll it.

## §6. GROWW-ORD-06 — intent ledger write failed

**Severity:** High. **Auto-triage safe:** Yes.

**Trigger:** the write-ahead NDJSON intent ledger
(`data/orders/groww-intents-<IST-date>.ndjson`, `GROWW_ORDER_LEDGER_DIR`) failed
an append / fsync, or replay found INTERIOR corruption. The mutation is already
REFUSED fail-closed (`LedgerUnavailable`: no durable intent, no send — persist
failures never `warn!`). A torn/corrupt FINAL line is tolerated (torn-tail rule
— a crash mid-append is the expected corruption, logged + counted); interior
corruption forces reconcile-only mode.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-06`; the `stage`
   (append / fsync / replay).
2. `df -h data/orders/` + `ls -la data/orders/` — disk-full / permission /
   mount issue is the dominant cause; the ledger dir must be writable.

## §7. GROWW-ORD-07 — unknown (open-set) status

**Severity:** Medium. **Auto-triage safe:** Yes.

**Trigger:** an order-status string OUTSIDE the case-insensitive open-set (the
12 annexure values + the undocumented `OPEN`) was observed. The order is PARKED
(no transition) + `tv_groww_order_unknown_status_total` + one coalesced warn per
DISTINCT string per day — the fail-closed answer to the O-1 status-drift
Unknown.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-07`; the verbatim raw
   status string.
2. A NEW distinct string is a day-0 harvest signal (the wire vocabulary drifted
   / a new value appeared) — add it to the parser's known set in a reviewed PR;
   never guess a rank for it.

## §8. GROWW-ORD-08 — order_audit write failed

**Severity:** Medium. **Auto-triage safe:** Yes.

**Trigger:** the shared `order_audit` QuestDB ILP append/flush failed
(best-effort forensic write — the AUDIT-WS-01 class). The intent ledger is the
safety record; a failed audit row NEVER gates a mutation.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-08`; the `stage`
   (ensure-DDL / append / flush). Run `make doctor` (cross-check BOOT-01/BOOT-02
   if it coincides with boot).
2. Re-appends are DEDUP-idempotent (feed-in-key); the next successful flush
   backfills.

## §9. GROWW-ORD-09 — quantity refused

**Severity:** High. **Auto-triage safe:** Yes.

**Trigger:** a requested order quantity exceeded the `max_order_quantity` config
gate (`GrowwOrdersConfig::max_order_quantity`, default 0 = refuse-all pending
the operator answer). Refused loudly BEFORE any HTTP — the fail-closed verdict
for Groww's absent slicing endpoint (the single-slice `plan_order_slices()` stub
never client-side-splits).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-09`; the requested qty
   vs the configured cap.
2. Raising the cap is a conscious config change; freeze-limit / exchange rules
   are exchange-published and changing — never hardcoded.

## §10. GROWW-ORD-10 — auth stale (minter-lock re-read ladder)

**Severity:** High. **Auto-triage safe:** Yes.

**Trigger:** an order-family call classified 401/403 (auth-stale — status +
shape, never body substrings). The access-token minter-lock re-read ladder
engaged: the token is RE-READ from SSM at a floor pacing, NEVER minted
(`groww-shared-token-minter-2026-07-02.md` — one minter, read-only consumers).
An in-flight mutation stays `ambiguous` with the resolution ladder clock PAUSED
(auth-stale time does not consume the 600s budget); the ladder resumes on token
recovery.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-ORD-10`. At the ~06:00 IST
   daily token reset this self-heals once the minter Lambda writes the fresh
   token; sustained 401/403 = the minter is stale (cross-check the shared-token
   minter runbook — never mint from here).
2. The paused-clock guarantee means a close-time ambiguity that hits auth-stale
   does NOT falsely exhaust into GROWW-ORD-03 during the auth outage.

---

## §11. State machine — rank-monotone open-set lattice

Order-status legality is **rank-monotone**, not a hand-enumerated table —
because a 2s/5s/15s poller SKIPS intermediate states, every legal multi-hop path
must be a legal single observed edge.

| Rank | Statuses (Assumed classification — no doc DAG exists) |
|---|---|
| 1 | NEW |
| 2 | ACKED |
| 3 | APPROVED, TRIGGER_PENDING |
| 4 | OPEN |
| 5 (in-flight request overlays) | MODIFICATION_REQUESTED, CANCELLATION_REQUESTED |
| 6 (terminal-fill) | EXECUTED |
| 7 (settlement) | DELIVERY_AWAITED |
| 8 (settled/closed terminals) | COMPLETED; and terminals REJECTED, FAILED, CANCELLED |

**Core rule:** any FORWARD move (`rank(to) > rank(from)`) from a LIVE source is
legal — this closes every poll-skip permutation (NEW→COMPLETED,
CANCELLATION_REQUESTED→COMPLETED, etc.). BACKWARD moves and moves OUT of a
terminal state are illegal ⇒ park + `needs_reconciliation` + GROWW-ORD-04 —
EXCEPT a backward observation from the RECONCILE snapshot source, which
classifies `stale_snapshot_skip` unless repeated on 2 consecutive sweeps.
Same-status = fill-field refresh only. Partial fills are DATA not states: a
`filled_quantity` monotonic ratchet (a decrease from a live source ⇒
GROWW-ORD-04; from reconcile ⇒ stale-skip). ANY transition into a terminal state
(or a terminal same-status refresh) with `filled_quantity > 0` ⇒ the
"position exists" High Telegram (`GrowwOrderCancelLostRace`) + audit
`event_kind=terminal_with_fill` — not just the EXECUTED race arm.

**Overlay-resolution arm (the rank-5 in-flight overlays):**
`*_REQUESTED → {ACKED, APPROVED, TRIGGER_PENDING, OPEN}` is LEGAL (the request
was absorbed and the order settled back to a working state); `*_REQUESTED → NEW`
is ILLEGAL (an order cannot un-acknowledge). Equal-rank laterals
(MODIFICATION_REQUESTED ↔ CANCELLATION_REQUESTED) are legal EXCEPT that
**`CANCELLATION_REQUESTED` is STICKY** — there is NO lateral OUT of a pending
cancel: a `CancellationRequested → ModificationRequested` observation does NOT
transition; it PARKS + reconciles (a modify cannot supersede a pending cancel).
`CANCELLATION_REQUESTED → EXECUTED|DELIVERY_AWAITED|COMPLETED` = cancel-lost-race
and fires the "position exists" page at ANY of the three observation points.

**Totality:** a table-driven test enumerates EVERY documented status × fill
state (0 / partial / full) × current state and asserts each lands on exactly one
of {legal transition, same-status refresh, park, stale-skip} — no pair implicit.
An undocumented status string ⇒ GROWW-ORD-07 (park + reconcile), never a panic.

---

## §12. EOD post-close classification (15:35 IST sweep — 4 arms)

Groww has NO `EXPIRED` status and the post-close wire status of a resting DAY
order is Unknown. At `GROWW_ORDER_EOD_SWEEP_IST` (15:35:00) one final reconcile
runs, then every non-terminal LOCAL order is classified into exactly one of four
arms (per-order polling then stops until the next session; the reads are the
bounded post-close carve-out of `no-rest-except-live-feed-2026-06-27.md` §10.7):

| Arm | Condition | Action |
|---|---|---|
| **Expired-local** | open, `filled_quantity == 0` | force-terminalize as Expired-class + audit row (mode-tagged); polling stops |
| **Force-expire-with-partial-fill** | open, `filled_quantity > 0` | force-terminalize Expired-class + the "position exists" High page (a partial position carries overnight) |
| **Filled-await-settlement** | a fill-class terminal (EXECUTED / DELIVERY_AWAITED / COMPLETED) | record terminal + settlement-awaited; no page |
| **Unknown** | status un-readable / open-set-parked at close | GROWW-ORD-03 Critical + operator page — NEVER a silent Expired stamp |

**Cross-close (process DOWN across 15:30):** a next-morning boot resolves
prior-date non-terminal intents PER-ORDER via status-by-reference / detail (NOT
the day-scoped `/v1/order/list`) — their prior-day availability is a day-0 probe
row; until proven, any prior-day intent whose fill state cannot be read routes
to GROWW-ORD-03 (a prior-day EXECUTED order is a live position), never a silent
Expired.

---

## §13. Delivery boundary (honest — no false-OK)

All ten codes are **log-sink-only today**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s "Which codes page" list (the paging drift
guard pins both untouched). The operator page for an order incident is the typed
Telegram event; the coded `error!` line is the forensic WHY. Adding a CloudWatch
log-filter alarm for any GROWW-ORD-* code is a flagged follow-up (one
`error_code_alerts` map entry + the doc paragraph + a cost note — the
SCOREBOARD-01 / FEED-REJECT-01 precedent).

**Honest envelope:** no live Groww order can leave the box today — four
independent gates (config default-OFF + absent cargo feature + hardcoded paper
mode + rule-file lock), each pinned by a build-failing ratchet
(`groww_order_lattice_guard.rs`). Paper mode exercises the ledger / FSM / audit
/ Telegram arms but NOT the HTTP classification / ladder / 429 / auth arms
(mock-integration mileage only until the future live flip).

---

## §14. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwOrd0*` / `GrowwOrd10*` variant)
- `crates/common/src/config.rs` (`GrowwOrdersConfig`) / `config/base.toml`
  `[groww_orders]`
- `crates/common/src/constants.rs` (`GROWW_ORDER_*`)
- `crates/core/src/notification/events.rs` (the `GrowwOrder*` events)
- `crates/common/src/broker_order_events.rs`
- Any future file under `crates/trading/src/oms/groww/`
- Any file containing `GROWW-ORD-`, `GrowwOrd0`, `GrowwOrd10`,
  `groww-intents-`, `GROWW_ORDER_`, or `groww_orders`
