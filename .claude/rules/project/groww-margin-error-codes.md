# Groww Pre-Trade Margin Surface — Error Codes (GROWW-MARG-01..05)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39.3 (the 2026-07-14 order-side area
> collision contract) > `no-rest-except-live-feed-2026-06-27.md` §10.2 (the
> KEEP-GATED endpoint table) > `websocket-connection-scope-lock.md` (2-WS lock
> untouched — this is REST-only) > this file.
> **Operator authorization (2026-07-14, the two grant rows, verbatim):**
> §10.2 margin row (`no-rest-except-live-feed-2026-06-27.md`):
> *"| `GET /v1/margins/detail/user` · `POST /v1/margins/detail/orders`
> (margin calculator) | REST | margin READ | **KEEP-GATED** —
> `[groww_orders] margin_read`, default-OFF + market-hours-only |"*
> §39.3 area row (`groww-second-feed-scope-2026-06-19.md`):
> *"| Margin (user + calculator) | `crates/trading/src/oms/groww/margin.rs` |
> `GROWW-MARG-*` |"*
> The area build itself was authorized in-session 2026-07-15 ("go, build the
> Groww margin area" — build-lead relay).
> **Companion code:** `crates/trading/src/oms/groww/margin.rs` (the §39.3-owned
> area file — snapshot poller + validator + pure `evaluate()` +
> carry-across-swap `PendingLedger` + 8-combo boot-guard, feature
> `groww_orders`), `crates/trading/src/oms/margin_gate.rs` (the 🔷 DHAN house
> pattern this mirrors — verdict/permits discipline, `to_paise` /
> `to_paise_signed` money helpers, exits-never-gated),
> `crates/common/src/config.rs::GrowwMarginGateConfig` (`[groww_margin_gate]`)
> (operator-held; today a module-local params struct in `margin.rs` — the
> config section lands with the area's config PR),
> `crates/common/src/error_code.rs::ErrorCode::{GrowwMarg01FetchDegraded,
> GrowwMarg02PersistFailed, GrowwMarg03SnapshotStaleGateClosed,
> GrowwMarg04EntryRejectedInsufficient, GrowwMarg05CalcDivergence}`.
> **Companion design:** the margin design record (design-final + REPLANT impl
> plan, 2026-07-14/15) — evaluation semantics (verdicts, buffer, ledger,
> staleness) are pinned there and ratchet-tested in margin.rs.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `GrowwMarg0*` variant verbatim —
> `GROWW-MARG-01`, `GROWW-MARG-02`, `GROWW-MARG-03`, `GROWW-MARG-04`,
> `GROWW-MARG-05` and the five variant names appear below.

---

## §0. Why these codes exist (the Groww funds floor + the fail-closed entry gate)

The Groww account is SHARED with the BruteX co-tenant and Groww documents
NEITHER balance freshness NOR margin-shortfall semantics — so "how much can an
entry spend right now?" has no authoritative real-time answer. The margin
surface answers it HONESTLY: a 60s in-session snapshot poll of the user-margin
detail endpoint (AND-gated: `[groww_margin_gate] enabled` AND
`[groww_orders] margin_read` — both default-OFF; market-hours-only per the
§10.2 grant row above; the whole subtree additionally sits behind the
non-default `groww_orders` cargo feature per the §39.2 lattice) feeds a RAM
snapshot; a PURE `evaluate()` turns `(snapshot, OrderIntent, pending-ledger,
config)` into a verdict — fail-CLOSED for entries on stale (> `stale_secs`),
absent, previous-IST-date, or bad-input data; **EXITS ARE NEVER GATED**
(`OrderIntent::Exit` is the only bypass, the margin_gate.rs house rule — an
exit must always be placeable even with the whole REST surface down). The
§38.8 decision-freshness gate applies verbatim: a stale snapshot is
RECORD-quality data, never a trading-decision input — the gate refuses rather
than evaluates on it.

**RAM-first divergence from the Dhan gate (deliberate):** the Dhan margin gate
issues per-entry REST calls (freshness by construction); the GROWW gate is
SNAPSHOT-CACHED by build-lead directive — the per-order decision NEVER does
I/O (one RAM read + one cold-path ledger sum), and freshness is enforced by
the staleness clock instead. The snapshot model is honest about its window:
the balance can change inside any 60s poll interval (BruteX co-tenant), which
is exactly what the buffer + ledger + floor bound.

**Shared-budget honesty (BUILD-LEAD DIRECTIVE, NOT grant text):** the §10/§39
grant rows carry NO cadence/self-cap number for Groww. The "reads self-capped
≤ 50% of the pooled Non-Trading budget" discipline applied here (60s poll =
~1 req/min ≪ the pooled Non-Trading 20/s · 500/min bucket shared with BruteX;
a governor belt at 1-per-30s as the defensive floor) is a **build-lead
directive mirroring the Dhan margin-gate house pattern**
(`[dhan_margin_gate] tenant_budget_percent ≤ 50` + the 5-of-assumed-20/s
self-cap, margin_gate.rs) — recorded here as such, per the gate-verify audit
that found the ≤50% wording absent from the Groww grant.

Five codes cover the taxonomy: fetch degrade (01), audit persist degrade (02),
staleness gate-closed (03), enforce-mode entry rejection (04 — the one
operator-judgment code), calculator divergence (05).

---

## §1. GROWW-MARG-01 — per-poll margin fetch degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already happened;
the next 60s poll re-attempts automatically — the operator inspects, never
manually re-fetches first).
`ErrorCode::GrowwMarg01FetchDegraded` (`code_str() == "GROWW-MARG-01"`).

**Trigger** (distinguished by the `stage` field):

1. `stage` ∈ `transport` / `timeout` / `status` / `auth` / `rate_limited` /
   `oversize` / `parse` / `failure_envelope` / `shape_incomplete` / `sanity` /
   `no_token` — the in-file fetch/validate ladder (margin.rs `FetchIssue` +
   `ShapeIssue` taxonomies): transport error or whole-poll timeout; a non-2xx
   status (429 → `rate_limited`, counted, NEVER out-polled; 401/403 → `auth` —
   HONEST BOUNDARY: the shared `TokenProvider` trait has exactly ONE method,
   so the poll has NO channel to signal the rejection back to a caching
   provider — the INJECTED provider MUST self-refresh on its own timer
   (time-based SSM re-read at ≥60s pacing, independent of failure signals;
   NEVER a mint — the token-minter lock); until it does, every poll fails
   `auth` and the gate CLOSES fail-closed at the stale edge — the safe
   direction, but there is NO stronger self-heal claim than that); a body
   over the size cap (`oversize`); a 2xx body that failed the whole-struct
   deserialize (`parse` — type drift fails the whole parse, the SEC-L2
   mechanics); a GA failure envelope or absent payload (`failure_envelope`);
   a parsed payload missing the decision field `option_buy_balance_available`
   (`shape_incomplete`); a sanity-clamp violation (negative / non-finite /
   above the ₹10-crore ceiling — `sanity`); or no access token at fire time
   (`no_token`). An invalid payload is NOT stored — the prior snapshot ages
   toward the stale threshold (shape drift and outage collapse into ONE
   fail-closed path, GROWW-MARG-03's).
2. `stage="escalation"` — the EDGE: 3 consecutive fully-failed polls. Fires
   ONCE per episode; re-armed by a successful poll (recovery = one Info line;
   the typed Telegram pair is the wiring PR — §7 below).
3. `stage` ∈ `token_read` / `client_build` / `task_respawn` — the APP-SIDE
   wiring legs (SSM token read failure, HTTP-CLIENT-01-class client build
   failure, supervised poll-loop respawn with
   `tv_groww_margin_task_respawn_total{reason}` — NOT live yet, lands with
   the app-wiring PR) — emit sites land with the app-wiring PR, listed here
   so the stage taxonomy is complete on day 1.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-01`; the payload
   carries `stage` + a bounded secret-redacted failure sample
   (`capture_rest_error_body`; **field NAMES only, never fetched balance
   values** — the SEC-M1 redaction rule for fetch-side lines).
2. `tv_groww_margin_fetch_total{outcome}` rates name the dominant class
   (`ok|error|rate_limited|auth|shape_drift`). Sustained `auth` outside the
   ~06:00 IST token-reset window = the shared minter Lambda is dead — see
   `groww-shared-token-minter-2026-07-02.md`. Sustained `rate_limited` =
   BruteX is exhausting the pooled bucket (U-5 scope Unknown) — never
   out-poll; record a dated note.
3. Sustained `shape_incomplete`/`parse` = Groww changed the payload nesting
   (the placement is Assumed until live-probed) — read the failure sample;
   open a shape follow-up.
4. Cross-check GROWW-MARG-03: 3+ failed polls means the snapshot is about to
   cross the 180s stale edge and close the entry gate.

**Honest envelope:** the fetcher guarantees a bounded, loud attempt per poll —
it cannot force Groww to serve fresh balances, and the account balance can
change inside ANY 60s window (BruteX co-tenant; freshness undocumented).
Missed polls degrade to the GROWW-MARG-03 fail-closed path, never to a stale
Pass.

---

## §2. GROWW-MARG-02 — margin audit persist failed

**Severity:** High. **Auto-triage safe:** Yes (best-effort forensic write;
gate decisions are RAM-only and UNAFFECTED — re-appends are DEDUP-idempotent).
`ErrorCode::GrowwMarg02PersistFailed` (`code_str() == "GROWW-MARG-02"`).

**Trigger:** the `margin_gate_audit` / `rest_fetch_audit` (`leg='margin_user'`
/ `'margin_calc'`) QuestDB leg failed (`stage` field): boot-time ensure-DDL
returned non-2xx / unreachable (`stage="ensure_client_build"` /
`stage="ensure_ddl"` — NOTE the HTTP-CLIENT-01-class consequence: the first
ILP write may auto-create the table WITHOUT DEDUP UPSERT KEYS, a duplicate-row
window until a later ensure succeeds), an ILP buffer append was rejected
(`stage="append"` / `stage="audit_append"`), or the ILP-over-HTTP flush was
refused by the per-request server ACK (`stage="flush"` / `stage="audit_flush"`
— the 2026-07-05 fire-and-forget lesson). On ANY failed flush the writer
DISCARDS its pending buffer (the shadow-writer `discard_pending` precedent;
`tv_groww_margin_persist_errors_total{stage}` — NOT live yet, lands with the
integration PR alongside these emit sites — counts) and the poll feeds the
GROWW-MARG-01 escalation edge (persist-gated — fetch-ok-but-lost-rows is not
"ok", audit Rule 11).

**Emit-site boundary (honest):** the audit tables live in `crates/storage`
(ABOVE trading), so these emit sites land with the INTEGRATION PR — the
variant ships now (contracts-first, the INSTR-FETCH-#9 sequencing) so the
integration PR compiles against a stable identifier.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-02`; the `stage`
   names the failing leg. Run `make doctor` (cross-check BOOT-01/BOOT-02,
   WAL-SUSPEND-01 — the sibling post-market writers share the QuestDB target).
2. **Day-1 DEDUP-verify checklist (the CHAIN-03 precedent):** on the first
   enabled boot, watch for `stage="ensure_ddl"` failures, then verify DEDUP
   engaged: `mcp__tickvault-logs__questdb_sql "select ts, security_id, verdict,
   source, count(*) c from margin_gate_audit group by ts, security_id, verdict,
   source order by c desc limit 5"` — every `c` MUST be 1.
3. Gate decisions were never at risk — only the queryable forensic rows for
   the outage window are missing; a later re-run UPSERTs in place.

---

## §3. GROWW-MARG-03 — margin snapshot stale, entry gate CLOSED

**Severity:** High. **Auto-triage safe:** Yes (the fail-closed refusal IS the
protection; the next fresh poll self-heals — the operator inspects the fetch
health, never force-opens the gate).
`ErrorCode::GrowwMarg03SnapshotStaleGateClosed` (`code_str() == "GROWW-MARG-03"`).

**Trigger:** edge-latched ONCE per staleness episode when the entry gate
transitions into a fail-closed state:

- snapshot age > `stale_secs` (default 180s = 3 missed polls), OR
- no snapshot at all (`FailClosedNoSnapshot`), OR
- the snapshot's `fetched_on_ist_date` is a PREVIOUS IST date (stale
  regardless of age), OR
- the 09:14 IST pre-open freshness check on a trading day: no snapshot
  FRESHER than `stale_secs` exists (a stale boot prime does NOT suppress it —
  the H L3 rule; the pre-open check itself lands with the app-wiring PR).

Re-armed on the falling edge (a fresh snapshot lands) — recovery is one Info
line (+ the typed `GrowwMarginFundsRecovered` Telegram in the wiring PR).

**Consequence:** entries are REFUSED (enforce) or recorded as would-reject
(observe); **EXITS ARE UNAFFECTED** — `OrderIntent::Exit` bypasses the gate
structurally (§38.8 verbatim: stale rows are never trading-decision inputs;
the fail-closed refusal is the gate honoring that, and an exit is never a
decision the snapshot gates).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the payload carries `age_secs` (or
   `reason="stale_age"`/`"no_snapshot"`/`"prev_ist_date"`). Cross-check
   GROWW-MARG-01 for WHY polls are failing (token / 429 / shape drift / Groww
   outage).
2. `tv_groww_margin_snapshot_age_secs` gauge (−1 before the first snapshot)
   — the live staleness trend. The snapshot's `fetched_at` is stamped at
   response-LANDED time (post-await), so the staleness clock starts when the
   data actually arrived — never up to ~15s early; `fetch_initiated_at` is a
   diagnostics-only latency companion, consumed by no decision path.
3. No manual action opens the gate: the next successful poll does. If the
   REST surface is healthy but polls are off, check `[groww_margin_gate]
   enabled` + `[groww_orders] margin_read` + market-hours (the poll only runs
   in-session, and only when BOTH flags are true).

**Honest envelope:** the stale threshold bounds OUR data age; it does NOT
bound BruteX drain inside the window — the buffer + ledger + floor bound that
(GROWW-MARG-04's arithmetic), and Groww's own order-reject is the final live
backstop.

---

## §4. GROWW-MARG-04 — entry rejected: insufficient usable funds (enforce mode)

**Severity:** High. **Auto-triage safe:** **No — severity-independent override
arm in `is_auto_triage_safe()`** (the FUTIDX-02 / WAL-SUSPEND-01 precedent,
contract-test-pinned by `test_groww_marg_codes_contract`): whether to free
funds, shrink sizing, re-tune the 20% buffer, or accept the no-trade is an
OPERATOR judgment over a shared account — auto-triage must never act on a
funds verdict.
`ErrorCode::GrowwMarg04EntryRejectedInsufficient` (`code_str() == "GROWW-MARG-04"`).

**Trigger:** ENFORCE mode only — the pure `evaluate()` returned
`RejectedInsufficient`:

```
usable = option_buy_avail_paise × (100 − buffer_pct) / 100     # default 20%
reject when required + pending_consumed_paise > usable
       OR (usable − required − pending_consumed) < floor_paise  # default ₹5,000
```

OBSERVE mode never emits this code: a would-reject is a `would_reject`
counter + audit row + `info!` — no page. `FailClosedBadInput`
(zero/negative/below-`premium_floor_paise` required amount) is ALSO not this
code — bad input is a caller-data fault, counted + audited under its own
verdict label, never dressed as insufficiency.

**`stage="ledger_full"` (the second emit site of this code):** a
`PendingLedger::push` refused at the 64-entry cap ALSO emits GROWW-MARG-04
with `stage="ledger_full"` + the counter
`tv_groww_margin_gate_ledger_full_total` — the caller MUST fail-close the
entry (an approval burst past the cap is itself a fault). A non-positive
push amount is refused typed (`LedgerPushRefused::NonPositiveRequired`) but
UNCODED — caller-data fault, never dressed as insufficiency.

**Episode latch (delivery boundary, honest):** the design pins the emission
episode-latched PER SID with re-arm on an intervening Pass for that SID (the
WS-GAP-10 pattern — two distinct same-day shortfall episodes page twice; a
rejection storm never spams). `OrderIntent` carries no SID, so the per-SID
latch lands with the RiskEngine wiring PR (which has the SID in hand); until
then the emit site is the bounded evaluation recorder in margin.rs — one
coded emission per enforce-mode rejection, bounded by the evaluation cadence
itself (per strategy signal / probe, never per tick).

**Balance numbers on this line (recorded decision):** the GROWW-MARG-04
emission carries the full paise arithmetic (`required_paise`, `avail_paise`,
`usable_paise`, `pending_consumed_paise`, `headroom_paise`,
`snapshot_age_secs`) — the decision is reconstructible to the paise, and the
operator needs the numbers to act (Telegram commandment 6). The SEC-M1
redaction rule (field names only, no balance values) applies to the
FETCH-side GROWW-MARG-01/03 lines and body samples, NOT to this deliberate
forensic verdict line.

**Triage:**
1. Read the paise arithmetic off the payload/audit row.
2. `mcp__tickvault-logs__questdb_sql "select verdict, source, count(*) from
   margin_gate_audit where trading_date_ist = today() group by verdict,
   source"` — a would-reject/reject trend vs a one-off.
3. Operator decisions (never automated): top up funds, reduce lots, re-tune
   `buffer_pct` from measured headroom data (dated note), or accept the
   no-trade day. Remember the account is SHARED — BruteX drain between polls
   is a named cause (compare consecutive snapshot-swap audit rows).

**Honest envelope:** the 20% buffer + carry-across-swap ledger
(**HOLD-EXPIRY-ONLY**, 120s default) + ₹5k floor BOUND — do not eliminate —
the shared-account TOCTOU; a rejection here is advisory-conservative and
Groww's own reject remains the live backstop for anything that slips
through. **Why the ledger never releases on a balance drop (hostile-review
C1/C2/C3, 2026-07-15):** on the shared account a snapshot-to-snapshot
balance decrement is UNATTRIBUTABLE (our fill vs a BruteX drain vs M2M),
and a same-snapshot approval cohort shares one baseline — one decrement
must never free multiple/foreign reservations, and an exit fill must never
free an entry reservation. So a reservation releases ONLY at hold-expiry
(or via the id-MATCHED fill hook once the wiring PR threads
`order_reference_id` into ledger pushes — an unmatched/id-less fill
releases NOTHING). Per-reservation reference ids are UNIQUE by
construction: `PendingLedger::push` REFUSES a duplicate `Some(reference_id)`
(`LedgerPushRefused::DuplicateReference`, refuter R1-1 2026-07-15), so a
REPLAYED terminal fill — poll re-observation of a completed order is the
NORMAL case, not an edge — can release at most the one matched entry,
never a sibling reservation. Over-reservation for ≤ `ledger_hold_secs` is the
fail-closed direction. The BruteX drain bound inside any window is
therefore: buffer + floor + the hold-expiry ledger — the snapshot is only
as fresh as the last poll.

---

## §5. GROWW-MARG-05 — margin-calculator divergence from the local formula

**Severity:** Medium. **Auto-triage safe:** Yes (visibility only — the
conservative local buffer stands; nothing is auto-re-tuned).
`ErrorCode::GrowwMarg05CalcDivergence` (`code_str() == "GROWW-MARG-05"`).

**Trigger:** the cold-path calculator verifier (probe-gated daily task,
default OFF; ONE 1-lot NIFTY ATM CE BUY body against the margin-calculator
endpoint, product-pinned to the intended live product string) found the
broker's total requirement diverging from our premium-with-buffers formula
beyond tolerance. One coalesced warn per day. The calculator is NEVER in the
order path (rate family Assumed, basket shape Unknown, RAM-first rule).

**Triage:**
1. Read the payload's `local_paise` vs `calculator_paise` + the divergence
   percent. A small stable divergence = charges-model drift (the buffer
   over-covers by design); a LARGE divergence = our formula or the Assumed
   `option_buy_balance_available` debit semantics is wrong.
2. Enforce-flip is BLOCKED until resolved — this code green ≥2 weeks is a
   pre-flip checklist item (design §6.5).
3. Divergence never auto-adjusts the buffer — a dated operator note re-tunes.

**Emit-site boundary:** the verifier is the PR-4-class follow-up in the SAME
margin.rs area file; the variant ships now for contract stability.

---

## §6. Shared-budget + co-tenancy record (build-lead directive)

| Discipline | Value | Status |
|---|---|---|
| Poll cadence | 60s in-session = ~1 req/min | margin.rs |
| Defensive belt | governor 1-per-30s floor (the interval IS the limiter) | margin.rs |
| Pooled bucket | Non-Trading 20/s · 500/min, SHARED with BruteX (~3 req/s empirical); pooling scope per-key-vs-account = Unknown (U-5) | documented |
| ≤50% self-cap wording | **Build-lead directive mirroring `[dhan_margin_gate]` (tenant_budget 50 / self-cap 5-of-assumed-20), NOT §10/§39 grant text** — recorded per the gate-verify audit | this file |
| 429 behaviour | count + skip to next poll, NEVER out-polled | margin.rs |

## §6b. Telemetry (shipped vs deferred — the complete list)

**Shipped with the margin AREA PR (margin.rs emit sites — branch
claude/groww-margin-gate):**

| Metric | Kind | Meaning |
|---|---|---|
| `tv_groww_margin_fetch_total{outcome}` | counter | per-poll fetch outcome (`ok|error|rate_limited|auth|shape_drift`) |
| `tv_groww_margin_snapshot_age_secs` | gauge | live snapshot age (−1 before the first snapshot; reset from the LANDED clock at each store) |
| `tv_groww_margin_headroom_pct` | gauge | **non-sensitive** funding signal: buffered usable balance as a % of the min-free floor, SATURATING at 100 — deliberately non-invertible, because the CW agent's prometheus collector ships the WHOLE `/metrics` endpoint into the CloudWatch metrics log group. **The raw account balance is NEVER shipped as a metric** (the earlier `tv_groww_margin_available_rupees` gauge was REMOVED 2026-07-15 — its "not CloudWatch-shipped" premise was false for the log leg; exact paise live only on the GROWW-MARG-04 verdict line, the §4 recorded decision) |
| `tv_groww_margin_gate_verdicts_total{verdict,source}` | counter | every evaluation's verdict (8 labels × `signal|reference_probe|selftest`) |
| `tv_groww_margin_gate_ledger_entries` | gauge | live reservation count (cap 64) |
| `tv_groww_margin_gate_ledger_full_total` | counter | cap-refused pushes (each is a GROWW-MARG-04 `stage="ledger_full"` line) |

**Deferred (named in this file, NO emit site yet):**
`tv_groww_margin_task_respawn_total{reason}` (app-wiring PR) and
`tv_groww_margin_persist_errors_total{stage}` (storage-integration PR).

## §7. Delivery boundary (honest — no false-OK)

All five codes are **log-sink-only today**: NO `error_code_alerts` map entry
in `deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list. The operator pages are the
TYPED Telegram edges (`GrowwMarginStaleGateClosed` /
`GrowwMarginFundsRecovered` / `GrowwMarginEntryRejected` /
`GrowwMarginCalcDivergence`) which are **deferred to the wiring PR**
(`crates/core/src/notification/events.rs` is a build-lead shared file) — until
that PR lands, the coded `error!` lines + counters are the operator surface.
The margin.rs emit sites carry REAL `code = ErrorCode::GrowwMarg0X.code_str()`
fields from day 1 (the variants + this rule file land via the contract-stubs
PR; the margin.rs emit sites land with the area PR — the atomic-slot
supersession of the margin_gate.rs:54-57 code-less interim). GROWW-MARG-02
and GROWW-MARG-05 have NO emit site yet
(storage-integration / PR-4-class follow-ups — contracts-first). The `.claude/
triage/error-rules.yaml` entries exist for all five (GROWW-MARG-04 as a real
escalate rule — its auto-triage override forbids an exemption). Adding a
CloudWatch log-filter alarm is a flagged follow-up (one map entry + a cost
note — the SCOREBOARD-01 / FEED-REJECT-01 precedent).

**Test-coverage envelope (honest):** the full `poll_turn`
fetch→validate→store composition IS test-driven against local mocks via the
private URL seam (success-path store + failed-poll-retains-prior pins,
2026-07-15 — the earlier untested-composition gap is CLOSED). What remains
untested is the `run_margin_poll_loop` WIRING beyond the gates-closed +
shutdown smoke (cadence, belt, the stale/escalation edge plumbing and their
`reason` labels) — the app-wiring PR's guard MUST add a loop-level
paused-time test driving one failure streak + one stale episode. Oversize
residual: a response WITHOUT a Content-Length (chunked/close-delimited) is
fully buffered before the post-read cap refuses — bounded by the 5s request
timeout (~bandwidth × 5s RAM worst case), the same accepted residual
tf-consistency documents for its /exec reads; both oversize arms
(pre-read declared + post-read undeclared) are mock-tested.

## §8. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwMarg0*` variant)
- `crates/trading/src/oms/groww/margin.rs`
- `crates/trading/src/oms/margin_gate.rs` (the shared money helpers)
- `crates/common/src/config.rs` (`GrowwMarginGateConfig` /
  `[groww_margin_gate]`) or `config/base.toml` `[groww_margin_gate]` /
  `[groww_orders]` sections
- Any file containing `GROWW-MARG-01`, `GROWW-MARG-02`, `GROWW-MARG-03`,
  `GROWW-MARG-04`, `GROWW-MARG-05`, `GrowwMarg01FetchDegraded`,
  `GrowwMarg02PersistFailed`, `GrowwMarg03SnapshotStaleGateClosed`,
  `GrowwMarg04EntryRejectedInsufficient`, `GrowwMarg05CalcDivergence`,
  `GrowwMarginSnapshot`, `GrowwMarginVerdict`, `PendingLedger`,
  `margin_gate_audit`, or `tv_groww_margin_`
