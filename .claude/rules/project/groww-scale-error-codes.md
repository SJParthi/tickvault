# Groww Auto-Scale Ladder — Error Codes (GROWW-SCALE-01..04)

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
> GrowwScale02GlobalHalve, GrowwScale03ShardOverlap, GrowwScale04AuditWriteFailed}`,
> `crates/core/src/feed/groww/shard_cutter.rs` (the fail-closed cutter),
> `crates/common/src/config.rs::GrowwScaleConfig` (`[feeds.groww.scale]`).
> Ladder emit sites land in PR-2 (`crates/app/src/groww_scale_ladder.rs` +
> `crates/storage/src/groww_scale_audit_persistence.rs`) per
> `.claude/plans/active-plan-groww-autoscale.md`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `GrowwScale0*` variant verbatim — `GROWW-SCALE-01`,
> `GROWW-SCALE-02`, `GROWW-SCALE-03`, `GROWW-SCALE-04` appear below.

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
fail-closed; the younger conflicting connection is killed. The `ticks` DEDUP
key `(ts, security_id, segment, capture_seq, feed)` still prevents duplicate
rows — this code is about the CONTRACT violation, not data loss.

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

## §5. Honest envelope (§F wording)

> "100% inside the tested envelope, with ratcheted regression coverage: the
> 16-row failure taxonomy of the auto-scale design maps every failure class to
> one of these four codes or an existing code (FEED-STALL-01,
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
- Any file containing `GROWW-SCALE-`, `GrowwScale0`, `cut_shards`,
  `groww_scale_audit`, or `tv_groww_scale_rollbacks_total`
