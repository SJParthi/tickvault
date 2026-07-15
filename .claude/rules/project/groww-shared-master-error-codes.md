# Groww Shared-Master Persist — Error Codes (GROWW-MASTER-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` (SAME shared tables, `feed='groww'`) >
> `data-integrity.md` "feed-in-key EVERYWHERE" (operator override 2026-06-28) > this file.
> **Companion code:** `crates/core/src/feed/groww/shared_master_writer.rs`,
> `crates/common/src/error_code.rs::ErrorCode::GrowwMaster01PersistFailed`,
> wired at the `Ok(set)` arm of `crates/app/src/groww_universe.rs` (the `[groww_universe]`
> daily rider — re-homed 2026-07-15 from the deleted `groww_activation.rs` live-lane watcher).
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this file to
> mention every `GrowwMaster01*` variant verbatim — `GROWW-MASTER-01` appears below.

---

## §0. Why this exists

PR #1229 put `feed` into the DEDUP key + DDL + row struct of the two SHARED master tables —
`instrument_lifecycle` (DEDUP `ts, security_id, exchange_segment, feed`) and
`index_constituency` (DEDUP `ts, index_name, security_id, exchange_segment, feed`) — and the
Dhan reconciler/boot stamp `feed='dhan'`. Per the operator's 2026-06-19 "SAME tables + feed
column" decision (no `groww_*` master table), PR-A adds the Groww-side writer that fills the
`feed='groww'` rows from the daily `GrowwWatchSet`, reusing the existing bulk ILP append fns.

The write is COLD-PATH, fire-and-forget, and best-effort: it is a forensic master/metadata
record, NEVER on the data-correctness or recovery path. A Dhan row and a Groww row for the
same `(security_id, exchange_segment)` are DISTINCT rows because `feed` is in the DEDUP key —
neither overwrites the other.

### 2026-06-29 — audit-coverage close-out: Groww now emits `instrument_lifecycle_audit` (feed='groww')

> **Operator directive 2026-06-29 (100%-audit-coverage charter + his direct question about the
> empty `instrument_lifecycle_audit WHERE feed='groww'` table):** the Groww shared-master writer
> persisted `instrument_lifecycle` DATA rows (feed='groww', ~767) + `index_constituency` (~742) but
> emitted ZERO `instrument_lifecycle_audit` TRANSITION rows — leaving no forensic "what changed in
> the Groww universe day-over-day" chain (only the Dhan reconciler wrote it).

This is now CLOSED. `persist_groww_instruments` first calls `emit_groww_lifecycle_audit`
(`crates/core/src/feed/groww/shared_master_writer.rs`) — **audit-first (§24)**, BEFORE the lifecycle
DATA UPSERT — which:

1. reads yesterday's `feed='groww'` snapshot (`build_groww_lifecycle_select_sql` →
   `parse_groww_lifecycle_dataset`, scoped `WHERE feed='groww' AND dry_run=false`),
2. diffs it against today's master set (`groww_today_attrs`) via the SAME pure
   `tickvault_storage::lifecycle_reconciler::classify_transition` the Dhan reconciler uses
   (appeared / updated / split / reactivated / expired / segment_moved / no-op; Day-1 empty prior ⇒
   all `appeared`; composite `(security_id, exchange_segment)` per I-P1-11),
3. appends one `instrument_lifecycle_audit` row per transition tagged `feed='groww'` via the existing
   `append_instrument_lifecycle_audit_rows`.

It is best-effort + degrade-safe: a prior-read OR append failure logs `error!(code=GROWW-MASTER-01)`,
bumps `tv_groww_master_persist_errors_total{stage="lifecycle_audit"}`, and RETURNS — NEVER aborting
Groww activation, the live feed, the data UPSERT, or any order/tick path; the next idempotent boot
re-emits (DEDUP UPSERT KEYS). `dry_run` skips the emission (§27 isolation). The pure
diff/parser/builders are unit-tested; the live diff-vs-yesterday needs a real QuestDB carrying
prior-day rows (boot-exercised).

### 2026-07-04 — boot-parity hardening (FIX 13): readiness gate + bounded retry + truncate ordering gate

Three hardening changes landed 2026-07-04 (branch `claude/groww-boot-parity-hardening`):

1. **Truncate ordering gate (FIX 13a).** The Dhan-lane one-shot ts-pin migration runs
   `TRUNCATE TABLE index_constituency` — QuestDB has NO row-level `DELETE ... WHERE feed='dhan'`,
   so the migration cannot be feed-scoped and could wipe just-written `feed='groww'` rows (both
   were unordered fire-and-forget boot spawns). A `MigrationGate` (AtomicBool + tokio Notify,
   `index_constituency_persistence::index_constituency_migration_gate()`) is now marked complete
   on EVERY migration exit path (outer-wrapper pattern), the migration itself moved to the TOP of
   `persist_index_constituency_mapping` (before the niftyindices CSV fetch, so the gate opens on
   every boot path), and `persist_groww_instruments` awaits the gate (bounded 120s) before its
   `index_constituency` append. A Groww-only boot (Dhan lane OFF → migration never spawns) times
   out once and proceeds — correct, since no truncate can run in that mode. Ratchets:
   `ratchet_migration_runs_before_csv_fetch` (app), `ratchet_constituency_append_waits_for_migration_gate`
   (core), `MigrationGate` unit tests (storage).
2. **Readiness gate + bounded retry (FIX 13b).** `persist_groww_instruments` first runs a QUIET
   QuestDB readiness probe (`SELECT 1` via `shared_probe_client`, 3 attempts, 5s backoff —
   deliberately NOT `wait_for_questdb_ready`, which emits BOOT-01/02 pages reserved for the boot
   path). A probe that never succeeds degrades with the SAME GROWW-MASTER-01 contract under the
   NEW `stage="readiness"` label on `tv_groww_master_persist_errors_total`. Each append stage
   (`lifecycle`, `constituency`) then gets bounded retry — 3 attempts, 2s/4s backoff — with the
   final failure keeping the exact prior degrade-safe semantics (error + counter +
   continue/return; activation and the live feed are never blocked). Triage is unchanged; a
   `readiness` stage rate means QuestDB was down at activation time — the next boot/activation
   re-runs idempotently.
3. **Boot Telegram Groww counts (FIX 13c).** `activate_groww_lane`'s watch-set `Ok(set)` arm now
   calls `feed_health.set_subscribed(Feed::Groww, resolved_stocks, indices)` at activation time
   (mirror of the Dhan subscription-plan call in main.rs), so the boot/readiness Telegram feed
   lines and the /feeds page show real Groww counts before the sidecar's first status report
   (which remains authoritative — idempotent overwrite). Ratchet:
   `ratchet_activation_sets_groww_subscribed_counts`.

### 2026-07-05 — MigrationGate hardening (delta-audit F13/F14/F15)

The 2026-07-05 delta audit confirmed three residual holes in the FIX 13a truncate-ordering gate:
(F13) the Groww writer's bounded 120s gate wait was shorter than the Dhan lane's realistic path
to the TRUNCATE on a Monday cold boot (the migration sat behind the §4 infinite-retry CSV boot +
TOTP + QuestDB wait + lifecycle reconcile), and the timeout `warn!` falsely claimed "Dhan lane
likely OFF; no truncate can run" on the dhan+groww profile; (F14) a runtime Dhan enable / D2b
lane cold-start retry ran the FIRST-EVER truncate hours after the Groww append — no bounded wait
at append time can order against that; (F15) the gate was one-shot per process while the truncate
was re-runnable per lane restart, so a best-effort marker-write failure (or the documented
delete-marker operator procedure) could re-TRUNCATE behind a permanently-green gate. Hardened
(branch `claude/migration-gate-hardening`):

1. **Process-global boot-prefix migration (F13+F14).** The marker-gated migration now runs from
   `main()`'s boot prefix (`index_constituency_boot::run_index_constituency_ts_pin_migration_at_boot`),
   spawned BEFORE the Groww activation watcher and regardless of `feeds.dhan_enabled` — a quiet
   12×5s QuestDB readiness probe (never BOOT-01/02 pages) then the gate-aware migration, marking
   the gate within ~75s worst case, inside the 120s wait, on EVERY boot mode. Ratchet:
   `ratchet_ts_pin_migration_spawns_before_groww_watcher` (main.rs source-order scan) +
   `test_ts_pin_readiness_constants_bound_inside_groww_gate_wait`.
2. **In-process exactly-once wrapper (F15).** `MigrationGate` gained a `run_once`
   `tokio::sync::OnceCell` latch; `migrate_index_constituency_truncate_once_with_gate` executes
   the TRUNCATE body at most once per process and SERIALIZES concurrent callers (no racing double
   truncate). A marker-write failure or mid-session marker delete can no longer re-truncate
   behind a green gate; the re-run happens at the next process boot, again ordered before the
   feed appends. Ratchets: `test_with_gate_skips_when_gate_already_complete`,
   `test_with_gate_runs_once_then_skips`, `test_with_gate_concurrent_callers_single_run`.
3. **Honest timeout wording (F13 triage misdirection).** The gate-timeout `warn!` now states the
   truth — the boot-prefix migration is delayed; the append proceeds best-effort and rows wiped
   by a later-landing truncate re-persist at the next boot/activation (this file's §1 envelope).
   Ratchet: `ratchet_gate_timeout_warn_is_honest` (the false premise is a banned literal).

---

## §1. GROWW-MASTER-01 — shared-master persist failed

**Severity:** Medium. **Auto-triage safe:** Yes (best-effort cold-path write; never gates
Groww activation, the live feed, or any order/tick path).

**Trigger:** `persist_groww_instruments` (spawned fire-and-forget from the Groww watch-set
`Ok(set)` arm) could not persist the Groww instrument set into `instrument_lifecycle`,
`index_constituency`, OR the `instrument_lifecycle_audit` transition chain via QuestDB
(ILP/HTTP down, QuestDB unreachable, flush error, prior-snapshot read error). The `stage` label
on `tv_groww_master_persist_errors_total` distinguishes `lifecycle` / `constituency` /
`lifecycle_audit` (the 2026-06-29 audit-chain emit, which can fail on EITHER the prior-snapshot
read OR the audit append) / `lifecycle_flip` (the 2026-07-05 disappearance state-flip below) /
`watch_build` (2026-07-15 — the daily rider's own `build_and_write_groww_watch` pull-until-success
loop failing past 3 attempts; NO new ErrorCode variant).

### 2026-07-05 — lifecycle-integrity fixes (disappearance flip + first_seen preservation)

Three audit-confirmed gaps closed in one PR:

1. **Disappearance now flips the DATA row (`stage="lifecycle_flip"`).** A Groww
   instrument that disappears from the universe previously kept its
   `instrument_lifecycle` master row `active` forever (it is absent from the daily
   UPSERT batch), and — because the DATA row never flipped — the audit diff re-emitted
   a DUPLICATE `expired` transition row EVERY day. `persist_groww_instruments` now
   applies a FEED-SCOPED in-place state flip
   (`instrument_lifecycle_persistence::update_lifecycle_state_for_feed`, WHERE
   `... AND feed='groww' AND dry_run=false`) for every non-active audit transition
   (`shared_master_writer::groww_disappearance_flips`), AFTER the audit append
   succeeded (§24 audit-first) — NEVER a DELETE (SEBI §25). A flip failure logs
   GROWW-MASTER-01 with the NEW `stage="lifecycle_flip"`; the audit row is already
   durable and the next boot's diff re-emits + re-flips.
2. **`first_seen_date` is PRESERVED across the daily UPSERT.** The prior
   `feed='groww'` snapshot (now loaded ONCE in persist and shared by the audit diff,
   the DATA UPSERT, and the flips) carries each row's prior `first_seen_date`;
   `resolve_groww_first_seen_nanos` keeps it on the full-row UPSERT instead of
   resetting it to today (mirror of the Dhan `resolve_first_seen_nanos` guard;
   SEBI forensic). A prior-snapshot READ failure now skips the audit + DATA UPSERT +
   flips together (writing without the prior would reset every first_seen) —
   the constituency append still runs.
3. **The Dhan warm-skip `last_seen` bump is scoped.**
   `build_bump_active_last_seen_sql` now carries `AND feed='dhan' AND dry_run=false`
   — previously it mutated `feed='groww'` + §27 dry-run rows.

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — look for `GROWW-MASTER-01`; the payload carries the
   `stage` (`lifecycle` / `constituency` / `lifecycle_audit` / `lifecycle_flip` /
   `watch_build` / `task_respawn`) and the error context. `watch_build` = the daily watch-set
   BUILD itself is failing (Groww master CSV / niftyindices fetch or parse — the rider keeps
   retrying with capped backoff; spot-leg VIX resolution + master continuity degrade until it
   succeeds); `task_respawn` = the supervised rider task died and was respawned
   (`tv_groww_universe_respawn_total{reason}`).
2. `tv_groww_master_persist_errors_total` rate non-zero → QuestDB ILP degraded; run
   `make doctor` (cross-check BOOT-01/BOOT-02 if it coincides with boot).
3. The Groww live feed, the `ticks` table, and trading are UNAFFECTED — only the queryable
   `feed='groww'` master/constituency rows are missing until QuestDB recovers. The append is
   idempotent (DEDUP UPSERT), so the next boot / next activation re-runs cleanly.

**Honest envelope:** the master/constituency write is best-effort. A sustained ILP outage
loses the `feed='groww'` forensic rows for the outage window; it never affects tick capture,
order placement, the Dhan master rows, or Groww feed activation.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::GrowwMaster01PersistFailed`
- `crates/core/src/feed/groww/shared_master_writer.rs::persist_groww_instruments`
- Reused bulk append fns: `instrument_lifecycle_persistence::append_instrument_lifecycle_rows`,
  `index_constituency_persistence::append_index_constituency_rows`,
  `instrument_lifecycle_persistence::append_instrument_lifecycle_audit_rows` (feature-gated
  `daily_universe_fetcher`).
- Audit chain (2026-06-29): `shared_master_writer::emit_groww_lifecycle_audit` (audit-first §24) +
  the pure `build_groww_lifecycle_select_sql` / `parse_groww_lifecycle_dataset` / `groww_today_attrs`
  / `build_groww_audit_rows`, reusing `tickvault_storage::lifecycle_reconciler::classify_transition`.
- Wiring: `crates/app/src/groww_universe.rs` (the watch-set `Ok(set)` arm of the daily rider —
  re-homed 2026-07-15; the live-lane `groww_activation.rs` was deleted with the Groww live feed).

---

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwMaster01*` variant)
- `crates/core/src/feed/groww/shared_master_writer.rs`
- Any file containing `GROWW-MASTER-01`, `GrowwMaster01`, or `persist_groww_instruments`
