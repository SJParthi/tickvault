---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/core/src/feed/groww/shard_cutter.rs"
  - "crates/common/src/config.rs"
  - "crates/app/src/groww_scale_ladder.rs"
  - "crates/storage/src/groww_scale_audit_persistence.rs"
  - "crates/app/src/groww_scale_lock.rs"
  - "crates/core/src/instance_lock.rs"
---

# Groww Auto-Scale Ladder — Error Codes (GROWW-SCALE-01..05)

> **⚠ RETIRED 2026-07-15 (Groww live feed deleted — operator Q1, received directly in this session: *"remove
> the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."*):** the auto-scale
> ladder, the shard cutter, the fleet lock, and every GROWW-SCALE-01..05 emit path are DELETED with the
> Groww live feed (the experiment was already dead per `groww-scale-aws-lockout-2026-07-06.md`; the code is
> now deleted outright, not dormant). The `GrowwScale0*` ErrorCode variants are RETAINED until the post-C4
> variant sweep (this file keeps satisfying the cross-ref test). Content below retained as historical audit.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §34 (the multi-connection
> authorization + tiers) > this file.
> **Operator directive (2026-07-03 evening, verbatim):** *"I planned to
> establish multiple connections… 100 parallel connections to test 100k
> instruments… every ten connection should hold 1k instruments per connection…
> incremental dynamic scalable connections of max 100 connections starting 10…
> when auto scale up happens if there is any failure how that can be auto
> corrected"* + Mac-first execution confirmed.
> **Companion code:** `crates/common/src/error_code.rs::ErrorCode::{GrowwScale01RollbackFired,
> GrowwScale02GlobalHalve, GrowwScale03ShardOverlap, GrowwScale04AuditWriteFailed,
> GrowwScale05DualFleetDetected}`,
> `crates/app/src/groww_scale_lock.rs` (the fleet dual-instance lock gate) +
> `crates/core/src/instance_lock.rs` (the named-lock machinery it reuses),
> `crates/core/src/feed/groww/shard_cutter.rs` (the fail-closed cutter),
> `crates/common/src/config.rs::GrowwScaleConfig` (`[feeds.groww.scale]`).
> Ladder emit sites land in PR-2 (`crates/app/src/groww_scale_ladder.rs` +
> `crates/storage/src/groww_scale_audit_persistence.rs`) per
> `.claude/plans/active-plan-groww-autoscale.md`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `GrowwScale0*` variant verbatim — `GROWW-SCALE-01`,
> `GROWW-SCALE-02`, `GROWW-SCALE-03`, `GROWW-SCALE-04`, `GROWW-SCALE-05` appear
> below.

---

## §0. Why these codes exist (the auto-correction contract)

The Groww auto-scale ladder grows the sidecar fleet 1 → 2 → 5 → 10 connections
(and, tier-gated, toward 100) only while every gate holds for
`gate_hold_minutes`. The operator's hard demand is that every scale-up failure
AUTO-CORRECTS — detect, rollback to last-healthy, backoff, retry — with zero
manual intervention. These four codes are the typed, pageable record of every
auto-correction decision, so the operator (and the `groww_scale_audit`
forensic table) can reconstruct every rung transition. PR-1 ships the
contract stubs (variants + this runbook); PR-2 ships the emit sites — until
PR-2 merges, ZERO production code emits these codes (intentional
contracts-first sequencing, same pattern as INSTR-FETCH Sub-PR #9).

## §1. GROWW-SCALE-01 — ladder rollback fired (rung failure auto-corrected)

**Severity:** High. **Auto-triage safe:** Yes (the rollback IS the
auto-correction — the newest connections were killed, the last KNOWN-healthy
rung restored, and an exponential hold `min(10m × 2^k, 4h)` scheduled before
the next attempt at the same rung).

**Trigger:** ADVANCING to rung N+step failed (spawn/subscribe failure on a new
connection, or the advance gate broke during PROBING before the rung was
verified). The ladder killed ONLY the newly added connections (the oldest,
verified connections are never touched), returned to `last_healthy` N, and
armed the hold.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-SCALE-01`; the payload
   carries `from_conns`, `to_conns`, `attempt` (k), `hold_secs`, `reason`.
2. `tv_groww_scale_rollbacks_total{reason}` rate — one-off rollbacks are the
   ladder working as designed; repeated rollbacks at the SAME rung mean Groww
   is refusing that connection count (the server-side cap the ladder exists to
   discover) — the ladder holds below it automatically.
3. `mcp__tickvault-logs__questdb_sql "select * from groww_scale_audit order by ts desc limit 10"`
   — the full rung-transition forensic chain (PR-2).

## §2. GROWW-SCALE-02 — fleet-wide failure: global cooldown + halve

**Severity:** High. **Auto-triage safe:** Yes (the cooldown + halve already
self-applied; the operator inspects whether the provider is throttling).

**Trigger:** ALL active connections failed within a 60-second window — treated
as an ACCOUNT-LEVEL throttle (provider rejecting us), NOT a single-rung
failure. The ladder applies a global 5-minute cooldown, HALVES the connection
count (`N → max(ceil(N/2), 1)`), staggers reconnects (never a thundering
herd), and resumes PROBING.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-SCALE-02`; payload carries
   the pre/post connection counts and the failure signature (auth-class vs
   close-class).
2. Cross-check the sidecar stderr for `GROWW LIVE FEED REJECTED` /
   `Authorization Violation` lines — an auth-class fleet failure at ~06:00 IST
   is the daily token reset (the token-minter lock's re-read path absorbs it);
   any other time points at entitlement / provider throttle.
3. Repeated halving down to 1 connection = Groww is rejecting multi-connection
   use — STOP raising the ladder; record the discovered cap with a dated note
   per §34.2 before any further tier work.

## §3. GROWW-SCALE-03 — shard overlap / coverage violation detected

**Severity:** Critical. **Auto-triage safe:** No (indicates a cutter bug —
never auto-actioned).

**Trigger:** the fail-closed invariant of the range-based shard cutter
(`crates/core/src/feed/groww/shard_cutter.rs::cut_shards` — shards DISJOINT,
union == watch-set, no duplicate `(exchange, segment, security_id)` identity)
was violated at a ladder step, OR the runtime duplicate-SID detector (PR-2)
saw the same instrument streaming from two shard files. The ladder step HALTs
fail-closed; the younger conflicting connection is killed.

**CORRECTED 2026-07-04 (Session-B verdict — the earlier DEDUP claim here was
REFUTED):** the `ticks` DEDUP key `(ts, security_id, segment, capture_seq,
feed)` does NOT prevent duplicate rows on cross-connection overlap —
`capture_seq` is globally unique and IN the key, so two connections streaming
the SAME instrument produce DISTINCT `capture_seq` values → two rows per tick.
Overlap = **silent duplicate-row data corruption** until the PR-2 runtime
duplicate-SID detector ships. The cut-time fail-closed cutter (`cut_shards`
disjointness + coverage assertion) is the ACTUAL protection — which is exactly
why this code is Critical, not Medium.

**Triage:**
1. This should be unreachable (the cutter ratchet tests pin the invariant) —
   a firing means a real cutter/ladder bug. Capture the payload's shard spec
   + conn ids and file it.
2. Freeze the ladder (`feeds.groww.scale.enabled = false` + restart) until the
   bug is fixed; the single-conn path is unaffected.

## §4. GROWW-SCALE-04 — groww_scale_audit row write failed

**Severity:** Medium. **Auto-triage safe:** Yes (best-effort forensic write —
mirror of AUDIT-WS-01; the ladder continues on in-memory state).

**Trigger:** the ladder could not persist a rung-transition row to the
`groww_scale_audit` QuestDB table (ILP down, QuestDB unreachable, disk full).
Ladder decisions are UNAFFECTED — the audit is a forensic side-record; only
the queryable row for that transition is missing until QuestDB recovers. The
one behavioral consequence: a restart during the outage window rehydrates from
the last PERSISTED rung (which is always a previously-verified healthy rung —
fail-safe, never an unverified one).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-SCALE-04`; run
   `make doctor` (cross-check BOOT-01/BOOT-02 if it coincides with boot).
2. The next transition re-appends normally (DEDUP UPSERT keys make replays
   idempotent).

## §4b. GROWW-SCALE-05 — dual scale-fleet instance detected (fleet spawn refused)

> Added 2026-07-04 (Session-B fix plan, operator go 2026-07-04 — "fixing and
> working?"). `ErrorCode::GrowwScale05DualFleetDetected`.

**Severity:** Critical. **Auto-triage safe:** No (Critical is never
auto-actioned; the refusal is the fail-closed protection itself).

**Trigger:** a scale-enabled boot (`make scale-test` / `scale-smoke` / any
boot with `feeds.groww.scale.enabled = true`, which runs with Dhan OFF and
therefore NEVER reaches the Dhan RESILIENCE-01 lock in `start_dhan_lane`)
attempted to acquire the **Groww-fleet dual-instance SSM lock**
(`/tickvault/<env>/instance-lock-groww-scale` — deliberately OUTSIDE the
banned `/tickvault/<env>/groww/*` namespace per the token-minter lock; reuses
the `instance_lock.rs` machinery via the named-lock knob) and was refused:

- **AlreadyHeld** — another tickvault instance (Mac + AWS, or two Macs) is
  ALREADY running a scale fleet against the SAME Groww account. Before this
  lock, that failure masqueraded as provider throttle (repeating close/auth
  rejects) with triage pointing at Groww, never at a peer instance.
- **SSM unavailable** — the bounded 3-attempt (2s/4s backoff, the same budget
  as the Dhan Step 6a-prime lock) retry exhausted. Fail-closed: we cannot
  prove there is no peer, so the fleet is refused.
- **Lock lost mid-run** — the fleet-lock heartbeat observed a foreign
  takeover (renewal returned not-owned) and exited after paging.
- **Renewals stale past TTL** — the heartbeat's SSM renewals failed
  CONSECUTIVELY for ≥ the full 90s TTL (3 × 30s — e.g. an SSM partition on
  the lock holder). Escalated ONCE per failure episode (the
  `consecutive_failures` field): a legitimately booting peer can take the
  stale slot and run a SECOND fleet while this holder's renewals keep
  erroring, and the mid-run loss page cannot fire until connectivity
  recovers — this escalation closes that previously-unpaged window
  (2026-07-04 adversarial-review MEDIUM fix). The heartbeat keeps retrying
  after the escalation.

**Delivery boundary (HONEST — no false-OK):** GROWW-SCALE-05 has NO typed
Telegram `NotificationEvent` variant and NO dedicated CloudWatch alarm yet.
Every emission is `error!` → the log-sink chain (stdout / app.log /
errors.log / errors.jsonl) → the generic ERROR→CloudWatch-log routing. At
the boot-gate stage the deferred Telegram notifier slot is unfilled by
design, so "pages Critical" here means **log-sink +
CloudWatch-log-derived alerting only** until a typed NotificationEvent
lands (tracked follow-up). `mcp__tickvault-logs__tail_errors` is the
authoritative surface for this code today.

**Consequence (degrade, NOT halt):** the multi-connection fleet + ladder +
shard bridges are SKIPPED for this boot; the boot falls back to the proven
SINGLE-CONNECTION Groww path (identical fallback semantics to a failed scale
PREFLIGHT — capture continues; only the multi-conn experiment is refused).
The rest of the app keeps running.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-SCALE-05`; the payload
   carries `peer` (the holder's `host_id`: `<aws-instance-or-local>:<pid>:<hex>`)
   or the SSM `error` string.
2. AlreadyHeld with a live peer: decide which host should run the scale test;
   stop the other (its heartbeat releases on shutdown, or the 90s TTL clears a
   crashed holder automatically). A repeating GROWW-SCALE-05 on a scheduled
   Mac scale test means the AWS box is ALSO scale-enabled — fix the config on
   the box, do not fight the lock.
3. SSM-unavailable: check AWS credentials/network on the host (`aws sts
   get-caller-identity`); the Groww token read uses the same SSM reachability,
   so a sustained outage will surface there too.
4. Corrupt lock JSON: `aws ssm get-parameter --name
   /tickvault/<env>/instance-lock-groww-scale`, then
   `aws ssm delete-parameter` after confirming no peer is live.

**Honest envelope:** the lock gates the FLEET spawn only (smallest safe
scope) — the single-conn Groww path is NOT gated, so two hosts each running
single-conn remain possible (pre-existing behavior, unchanged).

- **Clean shutdown RELEASES the lock** (2026-07-04 adversarial-review HIGH
  fix): the graceful SIGINT/SIGTERM path calls
  `GrowwScaleFleetLockGuard::release_on_shutdown()` inside
  `run_process_runloop` — bounded 5s wait for the SSM DeleteParameter — so
  a same-host restart (the dominant Mac scale-test iteration workflow)
  re-acquires IMMEDIATELY instead of seeing its own dead slot as a peer.
- **Hard crash (kill -9 / panic-abort) still relies on the 90s TTL**: a
  restart within that window sees AlreadyHeld, falls back to single-conn
  for the WHOLE session (the gate runs once at boot — no mid-run
  re-acquire), and fires a GROWW-SCALE-05 page whose `peer` is your own
  previous `host_id`. Recovery: wait ~90s after a crash, then restart.
- **Stale-takeover race**: two boots that BOTH observe the same stale slot
  can BOTH return Acquired (PutParameter overwrite has no compare-and-swap);
  the loser detects foreign ownership at its first renewal (≤30s) and pages
  — but its already-running fleet is NOT torn down, so the dual-fleet state
  persists until the operator stops one host (one Critical page, then
  coexistence — bounded-loud, not self-healing).
- **A mid-run foreign takeover pages but does NOT tear down an
  already-running fleet** (the collision is loud, never silent). Wiring the
  guard's `held` flag into a ladder freeze/teardown is the PR-2 follow-up.

## §5. Honest envelope (§F wording)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> 16-row failure taxonomy of the auto-scale design maps every failure class to
> one of these five codes or an existing code (FEED-STALL-01,
> FEED-SUPERVISOR-01, PROC-01, RESOURCE-01..03, BOOT-01/02); rollback always
> returns to a KNOWN-healthy rung; the cutter invariant is build-failing
> ratcheted. NOT claimed: Groww's server-side connection/subscription caps —
> the ladder discovers and holds below them; a provider-side permanent reject
> surfaces as a repeating GROWW-SCALE-02, which is the honest signal, not a
> silent workaround."

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwScale0*` variant)
- `crates/core/src/feed/groww/shard_cutter.rs`
- `crates/common/src/config.rs` (`GrowwScaleConfig`)
- Future `crates/app/src/groww_scale_ladder.rs` /
  `crates/storage/src/groww_scale_audit_persistence.rs` (PR-2)
- `crates/app/src/groww_scale_lock.rs` / `crates/core/src/instance_lock.rs`
  (the named-lock machinery)
- Any file containing `GROWW-SCALE-`, `GrowwScale0`, `cut_shards`,
  `groww_scale_audit`, `tv_groww_scale_rollbacks_total`,
  `acquire_groww_scale_fleet_lock`, or `instance-lock-groww-scale`
