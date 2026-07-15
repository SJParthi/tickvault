# 🔷 DHAN Exit-Order Layer Lockout — Error Codes (EXIT-ORDER-01 / EXIT-VERIFY-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `dhan-rest-only-noise-lock-2026-07-14.md` (the Telegram posture this layer
> complies with) > `websocket-connection-scope-lock.md` "2026-07-13 Amendment"
> (live trading re-wire semantics) > this file > defaults.
> **Scope:** PERMANENT. Every PR, every branch, every future Claude/Cowork
> session.
> **Operator context (2026-07-14, coordinator-relayed — recorded as relayed
> intent, not a verbatim quote):** the 🔷 DHAN exit-order execution layer
> (Cluster B — super orders, forever/OCO, slicing, MPP verify-after-place)
> is authorized to land code-complete but **DEFAULT-OFF behind four
> independent locks**; enabling the config flag activates **dry-run paper
> behavior only**; going LIVE requires the §4 enable-time protocol with a
> fresh dated operator quote recorded HERE first.
> **Mechanical enforcement:** `crates/trading/tests/dhan_exit_order_lockout_guard.rs`
> (12 build-failing tests; `All Green` is a GitHub-required check on `main`,
> so a red guard physically blocks the merge button — see
> `merge-gate-lock-2026-07-04.md`).
> **Plan mapping:** the session label "Cluster B" used throughout this
> layer's code/commits IS umbrella plan item **E1** of
> `.claude/plans/active-plan-dhan-order-surface.md` (the exit-order layer
> row) — one and the same work item.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `ExitOrder01*` / `ExitVerify01*`
> variant verbatim — `ExitOrder01ExecutionDegraded`, `ExitVerify01Degraded`,
> `EXIT-ORDER-01` and `EXIT-VERIFY-01` appear below. `runbook_path()` for
> both variants points at THIS file (test `every_runbook_path_exists_on_disk`).

---

## §0. Why this exists (the layer in one paragraph)

The exit-order execution layer appends six methods to the EXISTING
`OrderManagementSystem` (`crates/trading/src/oms/engine.rs`, the
`// ==== EXIT EXECUTION (Cluster B) ====` region): `place_super_order`
(3-leg entry+TP+SL in ONE POST — exchange-resident exits placed at entry),
`modify_super_order_leg`, `cancel_super_order_leg`, `place_forever_oco`,
`place_order_sliced` (freeze-limit escape hatch), and
`verify_order_execution` (MPP verify-after-place). Pure logic lives in
`crates/trading/src/oms/exit_rules.rs`; the app-side dispatcher / S6-G1
call-site hub is `crates/app/src/exit_execution.rs`, driven from the
`Signal::Exit` arm of the trading pipeline. **The account has NEVER placed
a live order** — every Dhan super/forever/slicing endpoint is
UNVERIFIED-LIVE (§7 ledger), so the whole layer ships paper-only, and every
lock below exists so it STAYS paper-only until the operator says otherwise
with a dated quote.

## §1. The lock contract — four independent locks

| # | Lock | Mechanism | What the guard pins |
|---|---|---|---|
| 1 | Config default-off | `ExitOrdersConfig { #[serde(default)] enabled: bool }` — an ABSENT `[exit_orders]` section is disabled (fail-safe); `config/base.toml` CARRIES the section with `enabled = false` (non-vacuous scan surface). `enabled = true` activates **dry-run paper behavior only** — never a silent no-op (one honest boot log line names the mode; Rule-14 #4 `enabled=false` trap closed) | guard tests 1 + 2 (config scan + serde/Default source pin + real empty-TOML deserialize) |
| 2 | No reachable live path | (a) the dispatcher's `!cfg.enabled` early-return drops every `ExitCommand` with `tv_exit_commands_dropped_total{reason="disabled"}` — no OMS touch; (b) the trading pipeline spawns only inside dhan-gated arms and `dhan_enabled = false` in base + production config — the whole pipeline is runtime-dead today; (c) even a spawned+enabled path cannot POST (lock 3) | guard tests 5 + 6 (dispatcher gate literal + deploy-path scan) |
| 3 | Hardcoded dry-run | engine.rs `new()` hardcodes `dry_run: true`; the ONLY setter is `#[cfg(test)] enable_live_mode`; every exit method's `if self.dry_run` branch sits in source order BEFORE any `get_access_token` call, with the `// LIVE-EXIT-ARM` sentinel between them | guard tests 3 + 4 (hardcode + cfg(test) gate + per-method dry-run-before-token source-order pin) |
| 4 | Build-failing ratchet | `crates/trading/tests/dhan_exit_order_lockout_guard.rs` + this rule file; `All Green` required on `main` | guard tests 9 + 10 (rule-file phrase pin + scanner self-test) |

Going live is therefore **≥3 PR-visible diffs** — delete the `dry_run: true`
hardcode (code), flip the config, edit the ratchet + this rule file with a
dated operator quote — each of which individually fails `All Green` if
attempted alone.

## §2. The guard — `dhan_exit_order_lockout_guard.rs` (12 tests)

| # | Test | Pins |
|---|---|---|
| 1 | `no_config_file_enables_exit_orders` | no `config/*.toml` sets `enabled = true` inside `[exit_orders]` (comment-stripping, section-scoped scanner); non-vacuous: base.toml carries the section with an explicit `enabled = false` |
| 2 | `exit_orders_config_default_is_off` | `ExitOrdersConfig.enabled` keeps the BARE `#[serde(default)]`; `Default` impl sets `enabled: false`; PLUS the real behavior: `Default::default()` and an empty-TOML deserialize are both disabled |
| 3 | `oms_dry_run_hardcoded_true` | `dry_run: true,` inside `OrderManagementSystem::new()`; no `set_dry_run` exists; every `fn enable_live*` is immediately preceded by `#[cfg(test)]` |
| 4 | `exit_methods_dry_run_before_token_fetch` | in all 6 exit methods: `if self.dry_run` < `// LIVE-EXIT-ARM` < `get_access_token` in source order (comment-stripped, sentinel-preserving) |
| 5 | `dispatcher_gate_literal_present` | `exit_execution.rs` carries `!cfg.enabled` + `tv_exit_commands_dropped_total` + `"disabled"` |
| 6 | `deploy_path_never_sets_exit_flag` | nothing under `deploy/**`, `.github/workflows/**`, or the `Makefile` sets the exit-orders flag (string + section-scoped TOML scan) |
| 7 | `exit_layer_emits_no_telegram_dispatch` | the exit-layer source (engine exit region + exit_rules.rs + exit_execution.rs) contains no `NotificationService` / `.notify(`; AND `NotificationEvent::OrderRejected` / `::CircuitBreakerOpened` keep ZERO production dispatch sites OR `dhan-rest-only-noise-lock-2026-07-14.md` §2 carries an `order execution` family row (the conditional pin that lets the legitimate rule-edited enable PR pass) |
| 8 | `validate_super_order_prices_wired_not_dead_code` | no `#[allow(dead_code)]` on `validate_super_order_prices`; a real call site exists inside the exit region |
| 9 | `lockout_rule_file_pins_the_contract` | this file exists and keeps the contract phrases (guard filename, lock table, enable-time protocol heading, both codes + both variant names) |
| 10 | `toml_scanner_detects_flipped_flag` | scanner self-test: flipped / off / commented / other-section / prefix-section fixtures + the M6a `toml`-crate doc-parser fixtures (inline-table `exit_orders = { enabled = true }` + dotted `exit_orders.enabled = true`) — tests 1 and 6 apply the doc parser to `config/*.toml` (must parse) and to parseable deploy-path files |
| 11 | `dry_run_false_literal_only_in_cfg_test_gate` | (M6b, 2026-07-14 hostile review) engine.rs carries EXACTLY ONE `dry_run = false`/`dry_run: false` literal (comment-stripped), and it sits within a `#[cfg(test)]`-gated item (the `enable_live_mode` body) |
| 12 | `exit_methods_only_called_from_dispatcher_and_engine` | (M6c) the S6-G1 only-caller pin — outside `crates/app/src/exit_execution.rs`, `oms/engine.rs`, and test code, ZERO production occurrences of `.place_super_order(` / `.modify_super_order_leg(` / `.cancel_super_order_leg(` / `.place_forever_oco(` / `.place_order_sliced(` / `.verify_order_execution(` |

## §3. Runbooks (both codes LOG-SINK-ONLY — the noise-lock posture)

**Noise-lock §2 compliance statement:** per
`dhan-rest-only-noise-lock-2026-07-14.md` §2, the ONLY Dhan Telegram alerts
are the 4-item family (spot-1m, option-chain, token-terminal, token-4h).
The exit layer adds **ZERO new Telegram emit sites**: both codes below have
NO `error_code_alerts` map entry, NO paging-list edit in
`observability-architecture.md`, NO terraform change. The operator surface
is coded structured logs (every line carries order/correlation ids) +
counters (`tv_exit_verify_probes_total{outcome}`,
`tv_exit_commands_dropped_total{reason}`). The engine's existing
`fire_alert`/`OmsAlert` seam stays sink-less (`set_alert_sink` keeps zero
production call sites); the `feed_badge()` arms for
`OrderRejected`/`CircuitBreakerOpened`/`CircuitBreakerClosed` are
RENDERING-ONLY prep (🔷 DHAN badged by construction IF a future rule-edited
PR wires the sink — zero emit sites today).

### EXIT-ORDER-01 — exit engine call degraded

`ErrorCode::ExitOrder01ExecutionDegraded` (`code_str() == "EXIT-ORDER-01"`).
**Severity:** High. **Auto-triage safe:** YES (the degrade already happened;
the dispatched command was refused/failed loudly — the operator inspects,
nothing is auto-re-placed). **Delivery:** log-sink-only.

**Trigger:** an exit engine call failed — a validation refusal on a
dispatched command (finite-price / LIMIT-MARKET-only / lot-multiple /
trailing-bound / I-P0-03 expiry gate / relative price ordering), a Dhan API
error on the live arm, an ENTRY_LEG cancel refused post-fill
(`entry_leg_cancel_allowed == false` — the naked-position race U4 is NEVER
exercised), a super-order quantity above the freeze limit (typed refusal —
no sliced-super endpoint exists), or a slicing response anomaly.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `EXIT-ORDER-01`; the payload
   carries the order/correlation ids + the refusal reason.
2. A validation refusal = the CALLER (strategy/dispatcher) sent an illegal
   request — fix the request construction, never relax the rule.
3. A Dhan API error: DH-905/DH-906 class is NEVER auto-retried (fix the
   request/order); DH-904 rides the existing backoff ladder and never trips
   the circuit breaker (place_order precedent).
4. An ENTRY_LEG-cancel refusal post-fill is the fail-closed design working
   — cancel the exit legs individually instead; never bypass the gate.

### EXIT-VERIFY-01 — MPP verify ladder exhausted degraded

`ErrorCode::ExitVerify01Degraded` (`code_str() == "EXIT-VERIFY-01"`).
**Severity:** High. **Auto-triage safe:** YES (visibility — the order stays
tracked; policy actions like modify-to-aggressive/cancel-remainder are LIVE
actions deliberately NOT implemented in the dry-run layer).
**Delivery:** log-sink-only.

**Trigger:** the verify-after-place ladder (bounded ≤5 probes, 1/2/4/8/10s
backoff, every probe consuming the shared 10/s GCRA) reached a degraded
verdict. Ladder timing is WALL-CLOCK (`tokio::time::Instant` — probe RTT
included), and on the FINAL rung the elapsed value is FLOORED to
`mpp_verify_deadline_secs` (H1, 2026-07-14 hostile review) — so with the
default rung sum (25s) inside the 30s deadline, exhaustion ALWAYS
classifies through an at-limit arm instead of ending silently in-budget
Pending. Degraded verdicts:
- `PendingAtLimit` — MPP converted a MARKET order to LIMIT and it is
  RESTING on the book at/past the deadline (orders.md rule 18). NEVER
  assumed filled; the order stays tracked AND is flagged
  `needs_reconciliation` (H1).
- `PartiallyFilled` at budget — `needs_reconciliation = true` is set on the
  ManagedOrder; the remainder is NEVER silently forgotten.
- `Unknown { raw_status }` — an unparsable Dhan status, or
  `NOT_IN_SUPER_LIST` on the ladder's FINAL attempt (M2: a missing-from-
  list snapshot RETRIES the remaining rungs first — one snapshot is not
  proof of absence; a body-unparsable Unknown stays decisive at any rung).
  Fail-closed: never treated as filled. M2 refined (refuter round 1,
  2026-07-14): for `NOT_IN_SUPER_LIST` the `needs_reconciliation` flag +
  the EXIT-VERIFY-01 error fire only AT/PAST the deadline
  (`elapsed >= deadline` — the ladder's final-rung elapsed floor lands
  the last probe there); an in-budget sentinel is potential list lag and
  is neither flagged nor errored yet. A body-unparsable/other Unknown
  stays always-flagged at any rung.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `EXIT-VERIFY-01`; the payload
   names the order id, verdict, attempts, and elapsed seconds.
2. `PendingAtLimit`: check the resting price vs market; a manual
   modify/cancel decision is the operator's (live actions not shipped).
3. Partial-at-budget: the existing `reconcile()` pass + the tracked
   remainder are the recovery path; verify positions before close.
4. `Unknown`: capture the raw status verbatim — a new Dhan status string is
   a doc-drift signal (annexure rule 15: no panic on unknown).

**Honest envelope:** verify classifies on STATUS + fill quantities ONLY,
never on the order-type echo (the book may show LIMIT for a request sent as
MARKET — MPP conversion). A probe HTTP failure/429 is inconclusive — it
retries the next rung and NEVER marks an order absent. `close → EXECUTED`
is never promisable under MPP; the ladder's degraded verdicts are the
honest terminal states. In dry-run, `PAPER-*` orders return
`SimulatedFilled` deterministically (no HTTP; GCRA still consumed) — a
verdict deliberately distinct so no consumer confuses paper for real.
**Pipeline blocking (H3, 2026-07-14 hostile review; H3c precision,
refuter round 1):** while `[exit_orders]` is ENABLED, the strategy task
drives the dispatcher INLINE from its select loop — a close order stalls
that task for up to ~`mpp_verify_deadline_secs` of LADDER SLEEP time,
with the CloseAll multi-slice case bounded by ONE overall verify budget
of the same deadline of ladder sleeps (remaining slices get one sleepless
at-limit probe each). Per-probe HTTP RTTs + GCRA pacing are ADDITIONAL to
that budget and scale with slice count — a large slice count adds
un-slept probe time on top. Ticks buffer in the broadcast channel
meanwhile and order updates may lag. Accepted for the dry-run layer —
flagged as a pre-live design change (§4 step 5: spawned exit executor).
Sliced-response honesty (M3): the server split is never echoed back, so
EVERY live sliced-tracked order carries `needs_reconciliation = true`
(single-object and count-match arms included) and the response Vec is
bounded at 1,000 rows (H3b — beyond-cap rows are dropped with a coded
anomaly log).
**Pre-existing residuals (refuter round 1, 2026-07-14 — documented, not
introduced by this layer):** (a) `handle_order_update`'s re-index
staleness — when a WebSocket update arrives via correlation_id with a NEW
`order_no`, the order is re-indexed (cloned) under the new key, and the
OLD-key entry can go stale afterwards; the H2 cancel gate consults the
tracked entry under the id the caller holds, so a fill recorded only on
the other key could read pre-fill — the PartTraded/terminal arm still
refuses cancels whenever EITHER consulted surface (super registry raw
status or the tracked entry) shows a fill. Unifying the re-index is a
Cluster A follow-up (that session owns `handle_order_update` internals).
(b) Untracked-entry fail-open — the H2 gate falls through to the
registry-only check when `self.orders` has no entry for the id (a
registry-tracked super with no ManagedOrder mirror): the
`entry_leg_cancel_allowed` raw-status gate remains the sole defense on
that shape.

## §4. The enable-time protocol (each step a separate PR-visible diff)

A future operator-enable PR sequence MUST, in order:

1. **Dated operator quote FIRST** — recorded verbatim in THIS file (and the
   REJECT list in §5 updated for the newly-legal surface). No verbal-only
   approvals (house law).
2. **Noise-lock rule edit** — `dhan-rest-only-noise-lock-2026-07-14.md` §2
   gains an "order execution alerts" family row (its own §3 law requires
   the rule edit before any new Dhan page). Only THEN may an
   `OmsAlertSink` → `NotificationEvent` bridge be wired in the app crate —
   guard test 7's conditional pin passes exactly when that row exists, and
   blocks any earlier sink wiring.
3. **Config flip** — `[exit_orders] enabled = true` (+
   `default_freeze_limit_qty >= 1` and a fresh `freeze_limits_reviewed_on`
   date ≤ 90 days old, verified against the NSE qtyfreeze file — the
   staleness WARN fires at pipeline init otherwise). This step alone still
   yields **dry-run paper mode only**.
4. **Going LIVE (the code diff)** — delete the hardcoded `dry_run: true` in
   `OrderManagementSystem::new()` AND update the guard rows it pins (tests
   3 + 4) AND this rule file, all in the same PR. A PR that deletes the
   hardcode without the ratchet + rule edits fails `All Green`; a PR that
   edits the ratchet without the dated quote here fails review per §5.
5. **Pre-live prerequisites (flagged, not shipped in Cluster B):** the
   reserved protective-op rate lane (cancel/SL-modify never denied by a
   probe burst), the 7,000/day `DailyRequestTracker` budget tracker, and
   a **SPAWNED exit executor** (H3, 2026-07-14 hostile review — the
   enabled dispatcher currently runs INLINE on the strategy task, which
   blocks the pipeline select loop up to ~`mpp_verify_deadline_secs` of
   ladder sleep per close, PLUS per-probe RTTs + GCRA pacing that scale
   with slice count (H3c); live trading must move the verify ladder off
   that task).

## §5. What a violating PR looks like (REJECT)

- Flips `[exit_orders] enabled` to `true` in ANY tracked config file, env
  var, deploy script, or workflow without the §4 protocol.
- Changes the `ExitOrdersConfig.enabled` serde/`Default` default away from
  `false`.
- Deletes the hardcoded `dry_run: true`, adds a live-mode setter outside
  `#[cfg(test)]`, or moves a token fetch ahead of the `if self.dry_run`
  branch in any exit method.
- Removes or weakens any of the 10 guard tests, the `// LIVE-EXIT-ARM`
  markers, or this rule file without a fresh dated operator quote HERE
  first.
- Wires a Telegram sink (`set_alert_sink`, an `OmsAlertSink` bridge, or any
  `NotificationEvent` dispatch for the order-path variants) without the
  noise-lock §2 family row landing FIRST.
- Exercises an ENTRY_LEG cancel after the entry is PART_TRADED/TRADED
  (bypassing `entry_leg_cancel_allowed` — the U4 naked-position race).
- Auto-slices a super order (no sliced-super endpoint exists — oversize
  super quantity is a typed refusal, never a silent split), or calls
  `/orders/slicing` for an under-freeze quantity (U5 — undocumented).
- Hardcodes a numeric NSE freeze quantity in Rust (operator config only).
- Adds an `error_code_alerts` entry / terraform alarm for `EXIT-ORDER-01`
  or `EXIT-VERIFY-01` without the noise-lock rule edit (they are
  log-sink-only by contract).

Any such PR MUST be rejected in review even if the operator approves
verbally — the operator must update this rule file FIRST with a dated
quote, only then can the PR land.

## §6. Honest scope notes

- **Forever/OCO is CNC/MTF-only** (07b §1) — it structurally CANNOT serve
  intraday F&O exits. `place_forever_oco` is wired for completeness per the
  mandate and is NEVER routed as the intraday exit vehicle; the intraday
  exit vehicle is the super order (exchange-resident TP+SL at entry) with
  `place_order_sliced` + verify as the close-all path.
- **Cap-at-freeze doctrine:** strategy code caps per-signal quantity at the
  freeze limit so the Super Order path is the norm; slicing is the escape
  hatch (warn-logged when actually used). Freeze quantities are
  operator-supplied config (`default_freeze_limit_qty` + the
  `freeze_limits_reviewed_on` staleness WARN) — never hardcoded; the
  per-underlying map is deferred to the entry-layer cluster (the layer that
  knows underlyings).
- **No QuestDB order-audit tables:** they were deleted 2026-05-20; the
  interim audit record is coded structured logs with correlation ids
  (SEBI rebuild is a separate cluster). Stated plainly — not camouflaged.
- **State machine:** exactly 4 new transitions into `Closed`
  (Pending/Confirmed/PartTraded/Triggered → Closed); NONE into `Triggered`;
  `Traded` stays terminal. The super-order TOP-LEVEL status walk is tracked
  as raw strings on `ManagedSuperOrder`, deliberately bypassing the
  ManagedOrder state machine.

## §7. Live-probe ledger (UNVERIFIED-LIVE — resolve before/at go-live)

| # | Unknown |
|---|---|
| U1 | `price` value for a MARKET super order under MPP (we require 0.0, mirroring the plain-order rule) |
| U2 | Super list `averageTradedPrice` wire typing (deserialize f64 + default, treat 0 defensively) |
| U3 | Slicing single-vs-array on the wire; correlationId echo per slice; `/orders/external/{cid}` behavior for a shared cid (mitigated by `SlicingResponse`) |
| U4 | Post-entry-fill ENTRY_LEG cancel effect on live exit legs — NEVER exercised by design (`entry_leg_cancel_allowed`) |
| U5 | `/orders/slicing` behavior for under-freeze quantity — never exercised (`requires_slicing` gates) |
| U6 | Which cancel response shape fires live (202-empty vs 200-JSON) — both handled now |
| U7 | Current NSE freeze quantities — operator config + staleness WARN; validate() only sanity-checks ≥ 1 |
| U8 | Whether a TARGET_LEG-only modify (no trailingJump field) cancels the SL trail — verify trail state after ANY TARGET_LEG modify |
| U9 | MPP interplay with the super-order ENTRY leg (does a MARKET entry inside a super also auto-convert?) — verify-after-place applies to super entries too |
| U10 | EVERY live endpoint behavior — the account has never placed a live order |

Plus the rate-accounting assumptions: a super POST is assumed to debit 1
GCRA cell; order GETs are assumed to share the Order-API bucket (probes
consume the shared GCRA, conservative); the 25-modification cap is assumed
to span super-order legs collectively.

## §8. Auto-driver / Insta-reel explanation

> Sir, the juice shop just built its "sell and step out" machine — the one
> that will one day place the exit orders with broker Dhan. Today it is
> bolted shut with FOUR separate locks: the wall switch is OFF (config),
> the power line to the room is not even connected (the pipeline never
> runs), the machine itself is welded into practice mode (hardcoded
> dry-run — flip every other switch and it still only prints paper slips),
> and an inspector (the 10-test guard) stands at the factory gate refusing
> to ship any van that touches a lock. Even the practice slips stay quiet —
> no phone pings (the Dhan noise rule): problems go into the logbook with
> the order number on every line. Opening the locks needs the owner's
> signed, dated letter FIRST — and each lock needs its own visible key
> turn, so nobody can sneak the machine live in one quiet move.

## §9. Trigger / auto-load

Always loaded (path is in `.claude/rules/project/`). Reinforced on any
session editing:
- `crates/trading/src/oms/engine.rs` (the EXIT EXECUTION region),
  `exit_rules.rs`, `types.rs`, `state_machine.rs`, `api_client.rs`
- `crates/app/src/exit_execution.rs` / `crates/app/src/trading_pipeline.rs`
- `crates/common/src/config.rs` (`ExitOrdersConfig`) / `config/base.toml`
  `[exit_orders]`
- `crates/common/src/error_code.rs` (any `ExitOrder01*` / `ExitVerify01*`
  variant)
- `crates/trading/tests/dhan_exit_order_lockout_guard.rs`
- Any file containing `EXIT-ORDER-01`, `EXIT-VERIFY-01`,
  `ExitOrder01ExecutionDegraded`, `ExitVerify01Degraded`, `ExitCommand`,
  `place_super_order`, `LIVE-EXIT-ARM`, `tv_exit_commands_dropped_total`,
  or `exit_orders`
