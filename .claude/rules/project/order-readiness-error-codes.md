# Order-Readiness Gate + Order-Path Dhan Error Taxonomy — Error Codes (ORDER-READY-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `dhan-rest-only-noise-lock-2026-07-14.md` (the 4-item Dhan alert set — this
> file adds NO page) > `.claude/rules/dhan/annexure-enums.md` rules 11-12 >
> `.claude/rules/dhan/api-introduction.md` rules 7-8 > this file.
> **Operator directive (2026-07-14, coordinator dispatch):** (1) GET /v2/profile
> pre-trade validation (dataPlan Active, Derivative segment, >4h token headroom)
> wired into the order-path readiness gate; (2) full DH-901..910 + DATA-800..814
> order-path error taxonomy mapped to retry/backoff/halt policy + typed ErrorCode
> + 🔷 DHAN-attributed alert text; (3) DH-904 backoff ladder (10/20/40/80s) wired
> on the order client.
> **Companion code:** `crates/trading/src/oms/{order_readiness.rs,
> error_taxonomy.rs, api_client.rs, engine.rs}`;
> `crates/common/src/error_code.rs::ErrorCode::OrderReady01GateRefused`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> `ORDER-READY-01` + `OrderReady01GateRefused` verbatim — both appear here.
> **Auto-load trigger:** always loaded (path is in `.claude/rules/project/`).

---

## §0. Why this exists — the fail-closed pre-trade gate

Before a LIVE order is routed, the OMS consults a cached readiness snapshot
built by the EXISTING `TokenManager::pre_market_check()` — the exact 3-check
gate (`dataPlan == "Active"`, `activeSegment` contains `Derivative`, token
headroom > 4h; it already emits its own DATA-806 / AUTH-GAP-01 coded errors).
The per-order gate is an O(1) RAM read of that snapshot — **never a REST call
per order** (RAM-first, banned Cat-10). The snapshot is refreshed at 08:45 IST
pre-market and every 900s in-session. The dry-run / paper path is
**byte-identical and gate-free** — the gate applies ONLY to the live path, and
`dry_run` stays hardcoded `true` (OMS-GAP-06 pinned). A live path with no probe
installed / stale verdict / invalid profile / low headroom / OMS halt REFUSES
the order fail-closed — never sends it.

## §1. ORDER-READY-01 — live order refused by the readiness gate

**Severity:** High. **Auto-triage safe:** **No** — severity-independent override
in `is_auto_triage_safe()` (the CHAIN-01 / FUTIDX-02 / WAL-SUSPEND-01 precedent):
restoring dataPlan / Derivative segment / token is an operator/broker ACCOUNT
decision, and no auto-triage action may ever touch the ORDER path. Not Critical:
the refusal IS the protection (no order is sent), and the path is unreachable
today (`dry_run` hardcoded true) — pre-live plumbing, the WS-GAP-10 /
DHAN-LANE-03 class of fail-closed gate.

**Reason slugs** (`ReadinessRefusal::slug()`):

| slug | meaning |
|---|---|
| `no_probe` | refresher never ran / no `OrderReadinessState` installed — the boot-seam spawn is not wired (fail-closed by construction) |
| `stale` | last OK probe older than `ORDER_READINESS_MAX_AGE_SECS` (2100s = 2 missed 900s cycles + 300s margin) |
| `profile_invalid` | last probe found dataPlan not Active OR segment not Derivative OR (DATA-806) cross-poison |
| `token_headroom` | effective token headroom (decayed from the probe snapshot) `< ORDER_TOKEN_HEADROOM_MIN_SECS` (14400s) |

**Trigger arms:** (a) the per-order gate refusal in `engine.rs`
(`check_live_order_gates`), edge-latched — first refusal per episode is
`error!(code = ErrorCode::OrderReady01GateRefused.code_str(), ...)`, subsequent
`debug!` (audit-findings Rule 4); (b) the refresher's Ready→NotReady rising edge
(`error!(code = ..., stage = "refresh_failed", ...)`).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `ORDER-READY-01`; the reason slug
   names the failing dimension.
2. Cross-check the companion DATA-806 / AUTH-GAP-01 coded errors the
   `pre_market_check` probe emitted for the underlying cause.
3. Dhan portal: confirm the data plan (`dataPlan == "Active"`) and the
   Derivative segment entitlement.
4. `tv_token_remaining_seconds` for `token_headroom`.
5. Refresher spawned? `tv_order_readiness_probes_total` rate 0 = the boot seam is
   not wired (`no_probe` fail-closure is expected until the handoff lands).
6. Halted (`OrderPathHalted`) ⇒ see the §2 row for the halting DH/DATA code;
   `clear_order_halt` or restart AFTER the account/token is fixed.

## §2. Order-path DH / DATA policy table

Per-endpoint policy, coded log, circuit-breaker interaction, and operator action
for every class. Place/Modify NEVER retry ambiguous classes; Cancel (exposure
reducing) gets exactly ONE 2s transient retry. A Dhan-shaped code always wins
over the raw HTTP status.

| Code | Class | Place | Modify | Cancel | ErrorCode logged | Trips CB | Operator action |
|---|---|---|---|---|---|---|---|
| DH-901 | Dh901 | rotate → retry once → HALT | same | same (halt set; cancel not gate-blocked) | Dh901InvalidAuth | no | fix token, `clear_order_halt` |
| DH-902 | Dh902 | HALT + alert | same | same | Dh902NoApiAccess | no | fix API access, clear halt |
| DH-903 | Dh903 | HALT + alert | same | same | Dh903AccountIssue | no | fix account, clear halt |
| DH-904 | Dh904 | backoff ladder | ladder | ladder | Dh904RateLimit | no | throttle; auto-recovers or gives up ~150s |
| DH-905 | Dh905 | never retry | same | same | Dh905InputException | yes | fix the request |
| DH-906 | Dh906 | never retry | same | same | Dh906OrderError (first real emit site) | yes | fix the order |
| DH-907 | Dh907 | check params, no retry | same | same | Dh907DataError | yes | fix params |
| DH-908 | Dh908 | never retry (ambiguous) | never retry (reconcile) | one 2s retry | Dh908InternalServerError | yes | reconcile — never blind resend |
| DH-909 | Dh909 | never retry (ambiguous) | never retry | one 2s retry | Dh909NetworkError | yes | reconcile |
| DH-910 | Dh910 | log + alert | same | same | Dh910Other | yes | inspect |
| 800 | Data800 | never retry | never retry | one 2s retry | Data800InternalServerError | yes | reconcile |
| 804 | Data804 | check params, no retry | same | same | Data804InstrumentsExceedLimit | yes | reduce instruments |
| 805 | Data805 | STOP-ALL 60s latch | same | same | Data805TooManyConnections | no | wait; auto-resumes |
| 806 | Data806 | alert-only + poison readiness | same | same | Data806NotSubscribed | no | check data plan; 900s refresh restores |
| 807 | Data807 | token refresh → retry once (no halt) | same | same | Data807TokenExpired | no | AUTH-GAP-05 self-heals |
| 808 | Data808 | token refresh → retry once (no halt) | same | same | Data808AuthFailed | no | same |
| 809 | Data809 | token refresh → retry once (no halt) | same | same | Data809TokenInvalid | no | same |
| 810 | Data810 | HALT + alert | same | same | Data810ClientIdInvalid | no | fix client-id, clear halt |
| 811..814 | Data811..814 | never retry | same | same | Data811..814 variants | yes | fix the request |
| HTTP 429, no shape | Http429NoCode | backoff ladder | same | same | Dh904RateLimit | no | as DH-904 |
| 401/403, no shape (WAF) | AuthNoCode | never retry | same | same | Dh910Other (NEVER Dh901) | yes | inspect gateway/WAF |
| 5xx, no shape | ServerErrorNoCode | never retry | never retry | one 2s retry | Dh908InternalServerError | yes | reconcile |
| other 4xx, no shape | ClientErrorNoCode | never retry | same | same | Dh905InputException | yes | fix the request |
| Dhan shape, unknown code | UnknownCode | log + alert (+sanitized raw code) | same | same | Dh910Other | yes | inspect |
| transport (`HttpError`) | Transport | never retry (ambiguous) | never retry | one 2s retry | Dh909NetworkError | yes | reconcile |

**Retry-safety justification:** Place/Modify retries are allowed ONLY for
provably-pre-processing refusals — rate-limit (DH-904/429; the limiter sits in
front of order routing) and, with a budget of exactly 1 at the engine layer,
genuine Dhan-shaped auth rejections (DH-901/807/808/809; auth precedes routing).
Anything that could have reached order processing (908/909/800/5xx/transport) is
NEVER retried on Place/Modify — the 25-modification-slot budget burns per
accepted modify and a partial application is ambiguous; recovery is
reconciliation, not resend. Every retry reuses the SAME request struct ⇒ SAME
correlationId (idempotency anchor). AuthNoCode (a non-Dhan-shaped 401/403 — a WAF
blip) is logged `Dh910Other`, NEVER `Dh901InvalidAuth`, so the pre-existing
DH-901 CloudWatch tripwire keeps its precision.

### §2b DH-904 ladder mechanics
`DH904_BACKOFF_SECS = [10, 20, 40, 80]` (150s total), driven by
`compute_dh904_backoff`. Each rung reuses the SAME request (same correlationId),
is circuit-breaker exempt, and re-checks the 805 stop-all latch. On exhaustion:
one `error!` + `tv_oms_dh904_exhausted_total` + a 🔷 DHAN "gave up" alert.

### §2c DATA-805 stop-all + DATA-807/808/809 refresh
805 engages a monotonic 60s process-local latch (`BrokerCooldownLatch`) checked
before every order call (place/modify/cancel); refusals are counted; passive
expiry, no background task; rising edge alerts once. 807/808/809 route the token
rotation through the EXISTING `TokenManager` renewal machinery (AUTH-GAP-05 owns
minting; the H3 ~125s mint cooldown + RESILIENCE-03 lock stay intact — the
trading crate NEVER mints), retry ONCE, and on a second failure return
`TokenExpired` WITHOUT halting (807 = refresh trigger, not a halt).

### §2d Halt semantics
DH-901 (post-retry) / DH-902 / DH-903 / DATA-810 engage an OMS-level halt latch
(a plain `Option<HaltInfo>` on `&mut self`, NOT process exit). Set is idempotent
(single alert). **`reset_daily()` does NOT clear the halt** — DH-902/903/810 are
account-level; an IST-midnight auto-clear would re-arm a dead account. Clearing
is deliberate operator action (`clear_order_halt`) or a process restart. Cancel
is EXEMPT from the halt latch and the readiness gate (checks ONLY the 805 latch):
cancel is exposure-REDUCING; blocking it on a stale profile is the dangerous
direction.

### §2e Circuit-breaker non-double-trip
Halt classes (901/902/903/810), rate classes (904/429/805) and refresh classes
(806/807/808/809) do NOT trip the OMS circuit breaker — the breaker's time-based
HalfOpen probe would fire a real order into a dead/throttled account; the halt
latch owns account-class recovery. Input/transient/anomaly classes keep tripping.
The pinned 429 CB-exemption test stays green.

## §3. Delivery boundary (HONEST — no false-OK)

Everything here is **log-sink-only**: ZERO new NotificationEvent variants, ZERO
`error_code_alerts` tf entries, ZERO observability-architecture.md paging-list
edits (noise-lock 2026-07-14 compliant). Alerting is (a) coded `error!`/`warn!`
logs with `code = ErrorCode::X.code_str()`, and (b) the EXISTING `OmsAlert` /
`OmsAlertSink` seam — which has **zero installers today (grep-verified)** so
nothing can page. Every `OmsAlert::operator_message()` string starts with
`🔷 DHAN`, so any future sink inherits attribution (noise-lock strategy). The
pre-existing DH-901 + DH-906 CloudWatch tripwires gain their FIRST real emit
sites here (working as designed — a genuine DH-901/DH-906 on a live order would
page via the approved filters; no noise-lock edit needed). **Live paging for
ORDER-READY-01 = FLAGGED FOLLOW-UP** requiring (1) a dated noise-lock §2 quote,
(2) a Cluster A NotifierAlertSink, and/or (3) a tf entry + doc paragraph + cost
note (the paging drift guard enforces all three). Because `dry_run` is hardcoded
true, ALL of this is pre-live plumbing, unreachable in prod today — stated
plainly.

## §4. Honest envelope

The gate reads a ≤35-min-old cached probe, NOT per-order broker truth —
mid-window entitlement kills are caught by the TAXONOMY at the order attempt
itself (DATA-806 poisons readiness; 807/808/809 refresh). The default token
rotation hook is a no-op: the retry-once reads the arc-swap's CURRENT token (the
renewal loop / watchdog may have already rotated); the second failure halts
promptly. Reconciliation — never a blind retry — is the recovery for
ambiguous-outcome classes (908/909/800/5xx/transport). Transport is treated as a
whole-class ambiguous error (the NeverSent/SentOrUnknown split is a flagged
follow-up requiring construction-site access to `reqwest::Error`).

## §5. Trigger / auto-load

This rule activates when editing:
- `crates/trading/src/oms/{order_readiness.rs, error_taxonomy.rs, api_client.rs, engine.rs}`
- `crates/common/src/error_code.rs` (the `OrderReady01GateRefused` variant)
- Any file containing `ORDER-READY-01`, `OrderReady01GateRefused`,
  `classify_dhan_error`, `evaluate_order_readiness`, `run_order_ladder`,
  `BrokerCooldownLatch`, `DATA_805_STOP_ALL_COOLDOWN_SECS`, or `tv_order_readiness`.
