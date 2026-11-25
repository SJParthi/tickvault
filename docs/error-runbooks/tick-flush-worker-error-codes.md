# Off-Thread Tick ILP Flush Worker — Error Codes (TICK-FLUSH-01) [REVIVED 2026-08-21]

> **RETIRED — full text (verbatim, incl. all operator quotes): `docs/rules-archive/tick-flush-worker-error-codes.md`.**
> Archived 2026-07-20 (context-size incident) — this stub keeps the runbook path alive.
> Code: TICK-FLUSH-01. **The 2026-07-17 retirement is OVER** — the variant was
> retained with its emit sites deleted, and it has a live emit site again as of
> 2026-08-21 (the tick spill drain supervisor). See the revival section at the
> end of this file; the archived text describes the ORIGINAL, deleted worker.

---

## REVIVED 2026-08-21 — TICK-FLUSH-01 has an emit site again

The "emit sites deleted" line above was true from 2026-07-17 until today. It is
now stale in one direction: the code is live again, from a different worker.

**The new emit site:** `tick_spill_replay::spawn_supervised_tick_spill_replay`
(`crates/storage/src/tick_spill_replay.rs`) — the supervisor for the tick spill
drain. It fires when the drain task exits or panics, then respawns it after
`REPLAY_RESPAWN_BACKOFF_SECS` (30s).

**Why this code rather than a new one.** TICK-FLUSH-01 means "the off-thread
tick ILP flush worker died and the supervisor respawned it". The spill drain is
an off-thread tick-ILP worker and its supervisor does exactly that, so a new
variant would be a near-duplicate whose only distinction is which of two
tick-ILP workers died — a distinction the log line already carries in `path`
and `reason`.

### What it means when it fires

The tick spill drain — the recovery arm of the tick rescue tier — stopped
running and was restarted. Spilled ticks are **not lost**: they are sitting in
`data/spill/ticks/*.ilp` as valid QuestDB line protocol. They are simply **not
queryable** until a drain round succeeds.

### Triage

1. `mcp__tickvault-logs__tail_errors` — find `TICK-FLUSH-01`; `reason` says
   `panicked` or `returned`.
2. `returned` means the HTTP client could not be built — check the preceding
   line, which names the builder error. This is the HTTP-CLIENT-01 class
   (TLS backend, DNS resolver, fd exhaustion), not a QuestDB problem.
3. `panicked` is a real defect in the drain; capture the backtrace.
4. Either way, check the backlog:
   `ls -la data/spill/ticks/` — non-empty `.ilp` files are unrecovered ticks.
5. Manual recovery remains available and unchanged:
   `curl --data-binary @data/spill/ticks/<file> http://<questdb>:9000/write`

### Honest envelope

A flapping drain is a symptom, not the disease. The spill files exist because
an ILP flush failed, and the drain failing on top of that means the rows stay
on disk. They are bounded by `TICK_SPILL_MAX_BYTES` (512 MiB); past that cap
the writer drops and counts, which is the pre-rescue behaviour — degraded, but
never worse than before the tier existed.

`tv_tick_spill_replayed_bytes_total` and `tv_tick_spill_replay_failed_total`
are **not EMF-selected and have no alarm**, so today this is loggable and
countable but not pageable. Adding a Dhan-scoped alarm needs a dated operator
row in `dhan-rest-only-noise-lock-2026-07-14.md` §2 first.
