---
paths:
  - "crates/app/src/order_runtime.rs"
  - "crates/app/src/oms_wiring.rs"
  - "crates/app/src/dhan_rest_stack.rs"
  - "crates/trading/src/oms/engine.rs"
  - "crates/trading/src/oms/types.rs"
  - "crates/trading/src/risk/engine.rs"
  - "crates/common/src/config.rs"
  - "config/base.toml"
---

# Dry-Run Order Runtime — Error Codes, Socket-Free Shape & Pre-Live Follow-Ups

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `websocket-connection-scope-lock.md` §A.1 +
> `dhan-rest-only-noise-lock-2026-07-14.md` (the order-update spawn is
> RETIRED; socket + WAL re-arm gated on a fresh dated quote) > this file.
> **Operator authorization:** coordinator directive 2026-07-14 (cluster A —
> order-side readiness audit): revive the dormant order machinery DRY-RUN
> ONLY — fill bridge, unrealized-P&L lot_size fix, Groww mark tap, alert
> sinks. NO live orders. **SOCKET-FREE re-scope (same day):** the merge with
> the operator Dhan noise lock (`dhan-rest-only-noise-lock-2026-07-14.md`)
> CUT the order-update WS spawn and the order-update WAL capture/drain/
> confirm from this PR — both are gated behind a fresh dated operator quote
> (§2 below is the retained live re-arm spec).
> **Companion code:** `crates/app/src/order_runtime.rs` (the single-owner
> actor), `crates/app/src/oms_wiring.rs`, `crates/app/src/dhan_rest_stack.rs`
> Phase 5b, the mark tap *(2026-07-17 truth-sync: originally the
> `groww_bridge.rs` per-tick tap — that file died with the Groww live feed
> (#1581, 2026-07-15); the marks were re-homed 2026-07-16 to the Groww
> per-minute REST legs' persist-confirm seam in `groww_spot_1m_boot.rs` +
> `groww_contract_1m_boot.rs`)*,
> `crates/trading/src/oms/{engine,types}.rs` (`FillEvent`),
> `crates/trading/src/risk/engine.rs` (`evaluate_daily_loss_halt`).
> **Companion plan:** `.claude/plans/active-plan-order-runtime-dryrun.md`.
> **Ratchets:** `crates/app/tests/order_runtime_spawn_site_guard.rs`,
> `dhan_rest_stack.rs::test_rest_stack_wires_order_runtime` (socket-free /
> WAL-free positive pins) +
> `test_rest_stack_spawns_no_order_update_ws_and_no_canary` (main's #1532
> negative ratchet — the socket ban),
> `crates/app/tests/dhat_mark_forward.rs`, `crates/app/benches/order_gate.rs`
> (+ `order_gate_mark_forward` in `quality/benchmark-budgets.toml`).

---

## §0. What this is (one paragraph)

With `[order_runtime].enabled = true` (base.toml ON; serde DEFAULT stays
OFF — an absent section boots byte-identical to the post-#1532 noise-lock
shape), the dhan-OFF REST stack's Phase 5b spawns a supervised
SINGLE-OWNER actor that owns an `OrderManagementSystem` (**dry_run
hard-true — no HTTP order call can exist**) + a `RiskEngine`. SOCKET-FREE:
no Dhan WebSocket is opened and no order-update WAL is captured/drained —
the runtime's order-update broadcast has ZERO producers today (paper fills
are synthesized in-actor; the gated live re-arm attaches the socket + WAL
producers to this same channel). It consumes
the order-update broadcast (Source=P filter), bridges fills into the risk
book via the widened `handle_order_update → Result<Option<FillEvent>, _>`,
consumes Groww marks through a bounded mpsc fed by an
`Arc<AtomicBool>`-gated tap *(2026-07-17 truth-sync: "per-tick" at ship
time — since the 2026-07-16 re-home the tap sits at the Groww per-minute
REST legs' persist-confirm choke points, ≤4 spot + ~30 contract marks per
minute)*, synthesizes next-mark paper fills,
evaluates the mark-to-market daily-loss halt, runs an honest reconcile
heartbeat ("broker reconcile SKIPPED — dry-run" + the REAL local
Σfills==net_lots invariant), sweeps pending paper orders at 15:30 IST,
resets at 16:00 IST, and proves the whole chain once a day via a gated
paper self-test. Zero new QuestDB tables, zero new ErrorCode variants, zero
new NotificationEvent variants.

## §1. Error-code usage per emit site (ZERO new variants)

| Emit site | Level + code |
|---|---|
| Reconcile mismatch / local Σfills≠net_lots divergence / severe receiver lag / mark-apply anomaly (reconcile class) | `error!` OMS-GAP-02 |
| Orphan fill-carrying update on an empty (fresh) book (unreachable until the live re-arm attaches producers; kept for the replay-across-restart shape) | `warn!` OMS-GAP-02 + `tv_oms_orphan_fill_updates_total` |
| Order rejected (OMS alert sink) / partial-lot fill remainder floored (engine) | `error!` OMS-GAP-01 |
| Circuit breaker opened (sink) | `error!` OMS-GAP-03 |
| Rate-limit exhausted (sink) | `error!` OMS-GAP-04 |
| Self-test failure / non-PAPER order id in dry-run / runtime respawn | `error!` OMS-GAP-06 |
| Risk halt (sink + the `trigger_halt` code fix) | `error!` RISK-GAP-01 |
| Sid-segment TRIPWIRE collision (see §3) / sid parse failure | `error!` RISK-GAP-02 |

Telegram: the alert sinks map onto the 5 EXISTING NotificationEvent
variants (OrderRejected, CircuitBreakerOpened/Closed, RateLimitExhausted,
RiskHalt). Metrics (all static labels): `tv_order_runtime_up`,
`tv_order_update_events_total{outcome}`, `tv_oms_orphan_fill_updates_total`,
`tv_risk_fills_recorded_total{kind}`, `tv_mark_forward_dropped_total`,
`tv_paper_fills_synthesized_total`, `tv_paper_fills_deferred_total`,
`tv_oms_reconcile_runs_total{mode}`,
`tv_oms_local_reconcile_divergence_total`,
`tv_paper_selftest_total{outcome}`, `tv_order_runtime_respawn_total{reason}`,
`tv_order_update_receiver_lagged_total`.

## §2. GATED — the live re-arm follow-up spec (order-update socket + WAL + alarms)

**NOT SHIPPED in this PR.** The merge with the 2026-07-14 operator Dhan
noise lock (`dhan-rest-only-noise-lock-2026-07-14.md` §3 + scope-lock §A.1)
cut the order-update WS spawn and the order-update WAL capture/drain/
confirm from cluster A. This section is the retained spec so the live
re-arm lands as ONE quoted follow-up unit — a single PR, gated on a fresh
dated operator quote in the noise-lock file FIRST, re-arming together:

1. **The order-update WS spawn** (`run_order_update_connection` from the
   REST stack) with the runtime's broadcast sender wired as the consumer —
   the runtime must subscribe BEFORE any producer starts (ordering law F5),
   and the socket gets an explicit frame-size cap (~1 MiB) via
   `WebSocketConfig` (tungstenite's None-config default is 64 MiB).
2. **Durable WAL frame capture** (`wal_spill = Some(ws_frame_spill)`) + the
   boot-staged order-update WAL drain into the broadcast BEFORE the socket
   spawns (FIFO law F4), with the CONDITIONAL whole-dir confirm: confirm
   iff the drain was parse-clean AND zero stale live-feed frames sit staged
   (design F6 — a whole-dir confirm with stale live-feed segments staged
   would archive them un-reinjected = silent tick loss); else ONE coalesced
   non-paging `warn!(code = WS-REINJECT-01,
   reason = "confirm_deferred_stale_livefeed")` + counter. Per-record-type
   confirm is IMPOSSIBLE (segment files mix record types).
3. **The two CloudWatch order-update alarms #1532 deleted** —
   `tv-<env>-order-update-ws-inactive` +
   `tv-<env>-order-update-reconnect-storm` (with its
   `order_update_reconnections_fallback` log metric filter).

Until then: the boot-staged order-update WAL segments remain the documented
Phase-A residual on dhan-off boots — undrained in `replaying/`, re-globbed
each boot, zero loss. **One-time operator archive procedure (optional
cleanup):** stop the app off-market; the stale segments in
`data/ws_wal/replaying/` predate the Dhan retirement (2026-07-13) and their
ticks are already in QuestDB (DEDUP-idempotent replays); move them aside
(`mv data/ws_wal/replaying/*.wal data/ws_wal/archive/`); restart.

## §3. I-P1-11 tripwire deferral (dated justification, 2026-07-14)

The OMS (`orders` map, order-id keyed) and RiskEngine (`positions` map,
`security_id: u64` keyed) predate the composite-key rule. Rewriting both to
`(security_id, exchange_segment)` touches the frozen-adjacent risk surface
and every call site — deferred. Instead the runtime keeps a FIRST-SEEN
SEGMENT mirror per sid: a fill or mark whose segment_code differs from the
sid's first-seen segment is SKIPPED with `error!` RISK-GAP-02 (a
cross-segment sid collision becomes a LOUD skip, never a silent P&L
merge). Honest bound: the colliding instrument's fills/marks are dropped
for the day — visible, never wrong.
**The composite `(u64, u8)` rewrite is a MANDATORY pre-live follow-up** —
no `dry_run = false` flip without it.

**Second pre-live landmine, same follow-up (E9, fix-round 2026-07-14):**
the paper book's id SPACE is Groww-native today — positions are keyed by
the u64 the MARK carried (bit-62 index ids / Groww exchange_tokens),
because in dry-run the fills are synthesized FROM the marks. In LIVE mode
positions would be keyed by DHAN-space sids (from real order updates)
while marks keep arriving in GROWW-space u64s — `update_market_price`
would never match a live position and unrealized P&L would read
permanently 0 (the daily-loss halt goes blind). The composite-key rewrite
MUST therefore include a cross-feed id MAPPING leg (contract identity per
`futidx-4-error-codes.md` §2 — never native-id equality). No
`dry_run = false` flip without BOTH halves.

**Third pre-live landmine, same follow-up class (post-#1562 audit,
2026-07-17 — ⚠ MANDATORY PRE-LIVE FOLLOW-UP):** the OMS reconcile's
non-terminal fill-drift arm (`crates/trading/src/oms/engine.rs::reconcile`,
the `order.traded_qty = update.traded_qty;` copy after the status refresh)
adopts the broker snapshot's `traded_qty` UNCONDITIONALLY — including
DOWNWARD, when a stale/lagging REST snapshot reports FEWER filled lots
than the WS-side updates already recorded. A downward copy lowers the
fill-delta baseline, so a later WS redelivery of the same fill computes a
POSITIVE delta again and emits a SECOND `FillEvent` — the RiskEngine
double-counts the fill (position + P&L corruption class). The C2 WS-side
path already carries a monotone guard; the reconcile arm does not.
Fix directions (choose one, live-mode work — do NOT invent semantics in
dry-run): (a) refuse the downward copy on non-terminal orders
(symmetric with the C2 WS-side monotone guard) + coded OMS-GAP-02
divergence log, or (b) adopt it but emit the correcting `FillEvent` so
the risk book stays consistent. UNREACHABLE TODAY: the dry-run path
returns before any broker REST reconcile (engine.rs ~:1271), so the
hazard is live-mode-only. The hazard site carries a matching in-code
comment block. **No `dry_run = false` flip without one of the fix
directions landed.**
**2026-07-18 update — direction (a) LANDED; this mandatory-pre-live item
is CLOSED:** the reconcile non-terminal arm now REFUSES the downward
`traded_qty` copy (mirroring the C2 WS-side monotone guard's comparison
semantics): the local `(traded_qty, avg_traded_price)` pair stands,
`needs_reconciliation` stays `true` (a refused correction is NOT
consumed), and a coded `error!(code = OMS-GAP-02)` divergence line names
the order id + local vs broker qty — emitted per refusal per reconcile
pass while the divergence persists (bounded by the reconcile cadence;
clears when the broker book catches up — the C8 upward-error precedent),
NOT edge-latched. Upward/equal copies and the
terminal arms are unchanged; the dry-run early return still precedes
every correction. Bite-proven test:
`live_mode_reconcile_refuses_downward_copy_no_double_fill`
(engine.rs test module — pre-fix it double-emitted a `FillEvent` on WS
redelivery; post-fix the redelivery is delta 0 / `None`).
Flagged pre-live follow-up: the non-terminal arm still ADOPTS the
snapshot's STATUS before the downward guard runs, so a snapshot proven
stale by the qty check can still regress the order's status
(pre-existing adoption; no double-count path — status is not a fill
baseline); refusing status alongside qty on a proven-stale snapshot is a
flagged pre-live follow-up. The terminal arm's silent unconditional
downward copy remains the documented B1 contract (pinned by
`live_mode_reconcile_terminal_applies_fill_but_never_status`) — out of
this guard's scope.

## §4. Scope guard — EXPLICITLY OUT (a violating PR is REJECTED)

1. Strategy/indicator activation — §28 boundary; the runtime never
   constructs IndicatorEngine/StrategyInstance (no signal source exists;
   the only order producer is the gated self-test).
2. Live mode ANYTHING: `enable_live_mode` stays `#[cfg(test)]`; `dry_run`
   hard-true (ratcheted by `ratchet_order_runtime_is_dry_run_hardcoded`);
   no config flag can flip it.
3. A second spawn site (main.rs / fast arm / trading_pipeline /
   groww_bridge *(deleted 2026-07-15 with the Groww live feed — the ban
   row stands for any successor mark-source module)*) — dual-OMS
   split-brain; ratcheted by
   `ratchet_order_runtime_spawned_only_from_rest_stack`.
4. `order_audit` / `pnl_audit` QuestDB tables — flagged follow-up (this PR
   adds NO tables).
5. HTTP command endpoints for the runtime — cut (attack surface); the
   heartbeat + metrics are the observability surface.
6. RiskEngine/OMS composite-key rewrite — §3 (mandatory pre-live).
7. Feed toggles, `[feeds.groww.scale]`, GDF — untouched.

## §5. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage:
> dry-run only — the OMS `dry_run` flag is hard-true with the only
> live-mode flip test-gated (build-failing ratchet); fills reach the risk
> book via an in-engine delta computation that is double-count-safe on
> same-status refreshes and duplicate updates (unit-pinned); unrealized
> P&L now multiplies lot_size (the pre-existing understatement bug is
> fixed + finiteness-guarded); the Groww mark tap costs ONE Relaxed
> load when disarmed (DHAT ≤1KiB/8 blocks over 10K calls; Criterion
> budget `order_gate_mark_forward` ≤100ns — hot-path-grade by design;
> 2026-07-17 truth-sync: the tap fires from the per-minute REST legs
> since the 2026-07-16 re-home, not per live tick). NOT claimed: marks
> are BEST-EFFORT (a full channel drops the mark, counted — the NEXT
> MINUTE CLOSE supersedes it, ~60s; 2026-07-17 truth-sync of the stale
> "the next tick supersedes it" claim — the honest recovery latency is
> a minute, not a tick; positions stay exact); the broker-side reconcile is
> SKIPPED in dry-run (the heartbeat says so honestly — the REAL invariant
> checked is the local Σfills==net_lots mirror); a runtime respawn starts
> a FRESH paper book (in-RAM state; replayed updates surface as loud
> orphan warns, never silent fills); cross-segment sid collisions are
> loudly SKIPPED, not composite-keyed (§3)."

**2026-07-18 truth-sync (marks re-homed a SECOND time — the cadence
cutover):** PR #1624's cadence cutover stood the legacy Groww per-minute
legs down (`[groww_spot_1m]` / `[groww_contract_1m]` enabled = false in
base.toml), which left the mark channel with ZERO producers — the sole
sender dropped at boot and the Fix-F arm read the closed channel as the
benign day-complete state (paper fills + P&L marks silently dead; the
daily paper self-test failed loudly at its 180s AwaitingMark timeout,
OMS-GAP-06 log-sink-only). The marks are now re-homed from the
stood-down legacy legs to the GROWW CADENCE EXECUTOR's spot
persist-confirm seam (`groww_cadence_executor.rs` — the own-fire close
forwarded after the spot_1m_rest flush ACK + fold handoff, the legacy
gating mirrored verbatim; ~4 spot marks per minute close). The Dhan
cadence executor is DELIBERATELY excluded: Dhan spot sids (13/25/51
IDX_I) are a different id space than the Groww-native u64s the paper
book keys on — cross-feeding would double-key instruments invisibly to
the §3 first-seen-segment tripwire (segments match across the split).
Honest residual: contract-leg OPTION marks remain dead on the cadence
path — the cadence lane fetches chains, not per-contract candles, so
only index spot marks flow. *(Closed at the PLUMBING level 2026-07-18
same-day: the chain cadence seam now forwards one mark per RESOLVED
option contract leg after the option_chain_1m flush ACK — identities
are REAL exchange_token u64s resolved via the day-cached
`resolve_groww_contract_books` machinery piggybacked on the
expiry-list master download (zero extra REST); an unresolved
strike/leg is counted (`tv_cadence_option_mark_unresolved_total`) +
skipped, never a synthetic id; only finite >0 LTPs forward, bounded
≤~1.1K try_sends/min vs the 8,192-default mark channel. PLUMBING ONLY
today — no option position can exist in paper mode (IDX_I-only
self-test, no strategy), so option marks match zero positions and
restore nothing today; they pre-wire option-leg unrealized P&L for
future option orders. Same day, order_observability.rs stopped
hardcoding `feed:"dhan"` on order_audit/pnl_audit rows —
`OrderSideWiring` carries the lane's feed discriminant. Honest
envelope: option-contract marks resolve only while the book expiry —
the flat nearest ≥ today from the Groww master — equals the cadence
fire's day-locked policy expiry, true today for NIFTY/SENSEX by
construction and for BANKNIFTY under the Assumed no-weeklies
monthly-only regime; on any divergence day (weeklies returning, or a
cross-broker `expiry_disagreement` Dhan-wins override re-keying the
Groww fire) 100% of the affected underlying's option marks are
counted-unresolved (`tv_cadence_option_mark_unresolved_total`,
fail-closed, never misattributed) until the cadence §3e book
delegation lands.)* The Fix-F
closed-channel warn now
distinguishes never-any-mark ("mark channel closed before any mark
arrived — no live mark producer is configured") from the benign
day-complete close; control flow unchanged. Ratchet:
`crates/app/tests/cadence_mark_source_guard.rs`.
MED-1 fold-in (same day): the closed-channel disarm arm now splits
THREE ways — the never-any-mark producer-less-boot warn; an abnormal
MID-SESSION producer death (marks flowed AND the close was observed
before the 15:30 IST close boundary the runtime's close-sweep already
uses) = a coded OMS-GAP-02 `error!` naming the mark count + the
`tv_order_runtime_mark_producer_lost_total` counter, log-sink-only
delivery boundary (no new alarm, no terraform change); and the benign
post-session day-complete warn. All three arms keep the identical
disarm-and-continue control flow (no respawn, no break).

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/app/src/order_runtime.rs` or `crates/app/src/oms_wiring.rs`
- `crates/app/src/dhan_rest_stack.rs` (Phase 5b)
- `crates/trading/src/oms/engine.rs` / `types.rs` (FillEvent),
  `crates/trading/src/risk/engine.rs`
- `crates/common/src/config.rs` (`OrderRuntimeConfig`) or `config/base.toml`
  `[order_runtime]`
- Any file containing `spawn_order_runtime`, `MarkForwarder`, `FillEvent`,
  or `tv_order_runtime_up`
