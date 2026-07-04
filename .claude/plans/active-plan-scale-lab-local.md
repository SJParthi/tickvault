# Implementation Plan: Groww 100K Max-Scale Lab (local-runtime branch ONLY)

**Status:** APPROVED
**Date:** 2026-07-04
**Approved by:** Parthiban (operator, 2026-07-04, verbatim): "check the dynamic
scaling connections and subscribing entirely the 100k instruments... the max to
check whether we can fetch the entire 100k or not" — authorized for the
LOCAL-ONLY Monday(Jul 6)–Wednesday(Jul 8) experiment on the `local-runtime`
branch. **This branch NEVER merges to main; main's scope locks (§34.2 tiers,
[100,1200] universe envelope, GROWW_MAX_UNIVERSE) stay untouched there.**
Operator update 2026-07-04 (relayed via coordinator): the experiment is
**PURELY GROWW** — "forget about dhan i wanted to check this purely for only
groww". No Dhan changes, no Dhan stub; the scale profile keeps Dhan OFF.

> Guarantee matrices: this plan cross-references the 15-row + 7-row matrices in
> `.claude/rules/project/per-wave-guarantee-matrix.md` and the §3 GUARANTEE
> CHECK of `.claude/rules/project/zero-loss-guarantee-charter.md` — all rows
> apply as on main EXCEPT: this is a leaf lab branch with NO GitHub CI (local
> validation is the only gate) and the [100,1200] universe envelope is
> deliberately bypassed for the SCALE LANE ONLY behind a new default-OFF flag.

## Design

The shipped §34 ladder (PRs #1386/#1387/#1388) ladders on **connection count**
(`GrowwScaleConfig.ladder: Vec<usize>`, rungs = conns) with a fixed per-conn
shard size `instruments_per_conn` (hard cap 1000 = Groww per-session cap).
`GROWW_SCALE_HARD_MAX_CONNS = 100`, so 100 conns × 1000 = 100K is expressible
in config. The actual ceiling is `effective_ceiling(target, shards.len())` —
the ladder subscribes what exists and halts at the universe-required conn
count (the built-in fail-closed "subscribe what exists" clamp).

The blocker: the watch set the ladder shards comes from the activation
watcher's `build_and_write_groww_watch` (Groww master IDX rows + NTM
constituents ≈ 767 entries, envelope-checked to `[100, 1200]`, live cap 1000).
So today the ladder can never need more than 1 connection.

Extension (all on this branch):

1. `crates/common/src/config.rs` — `GrowwScaleConfig` gains:
   - `full_master_universe: bool` (default `false`): scale lane builds its
     subscribe set from the ENTIRE Groww master CSV (~100K rows), bypassing
     the [100,1200] envelope for the SCALE LANE ONLY (single-conn path and
     master tables untouched).
   - `gate_max_mem_used_pct: f64` (default `85.0`): NEW memory watermark
     advance gate (the shipped set had CPU + disk only) so auto-abort trips
     before the host becomes unusable.
2. `crates/core/src/feed/groww/instruments.rs` —
   - pure `full_master_entries_from_csv(csv)`: every usable master row →
     `WatchEntry` (IDX → `IndexValue` + `stable_index_security_id`; others →
     `Ltp` + numeric-token security_id; non-numeric-token non-IDX rows are
     skipped + counted), deduped by `(exchange, segment, security_id)` (the
     shard cutter's fail-closed identity), deterministic sort matching the
     cutter's `(exchange, segment, security_id)` order.
   - `fetch_full_master_entries()`: hardened download of
     `GROWW_INSTRUMENT_CSV_URL` + the pure builder.
3. `crates/app/src/groww_scale_ladder.rs` —
   - `GateInputs` gains `mem_used_pct: Option<f64>`; `gate_failure_reason`
     gains the `mem_high` arm; `smoke_gate_inputs` carries it (memory stays a
     REAL gate in smoke, like CPU/disk).
   - Host probes become cross-platform: Linux (`/proc/loadavg`,
     `/proc/meminfo`) AND macOS (`sysctl -n vm.loadavg`, `vm_stat` +
     `sysctl -n hw.memsize`) via pure, unit-tested parsers; unavailable probe
     = gate honestly skipped (unchanged contract).
   - Full-master path: when `full_master_universe` is true the ladder fetches
     the full master set (3 bounded attempts, 10s backoff); on failure it
     falls back LOUDLY to the activation watch set (degrade, recorded).
   - End-of-stage summary emitter: on every VerifiedHealthy / RolledBack /
     GlobalHalved / HaltedAtCeiling / ProbeVerdict transition, one TSV row
     (stage conns | target | ceiling | shards | subscribe-proof conns |
     capturing conns | total tick bytes | cpu% | mem% | disk-free% | outcome)
     to `data/groww-scale/summary-<date>.tsv` + an `info!` mirror — the
     operator's Tuesday table.
4. `scripts/groww-scale-test.sh` — new modes `max` (full ladder
   `[1,2,5,10,20,40,80,100]`, target 100, per-conn 1000,
   `full_master_universe = true`) and `max-smoke` (same + weekend SMOKE);
   `DRY_RUN=1` skips docker/cargo so the harness dry-runs in any sandbox.
   Overlay keeps `dhan_enabled = false` (purely-Groww directive).
5. `Makefile` — `scale-max`, `scale-max-smoke` targets.
6. `docs/runbooks/groww-scale-test.md` — Monday 09:15→09:45 sequence, max
   mode, auto-abort semantics, evidence locations. No Dhan content.

## Edge Cases

- Master has fewer rows than target (e.g. 60K usable): `effective_ceiling`
  clamps; ladder halts at ceiling; summary + audit record actual vs target.
- Master rows with duplicate `(exchange, segment, security_id)` (Groww reuses
  numeric tokens across exchanges/segments): builder dedups BEFORE the cutter
  so GROWW-SCALE-03 never fires on upstream duplicates; skipped/deduped counts
  logged.
- Non-numeric `exchange_token` on a non-IDX row: skipped + counted (never a
  0-security_id entry that would collide in dedup).
- Full-master fetch fails (network/CDN): 3 attempts, then loud fallback to the
  ~767 activation set — the run degrades to the shipped Tier-A behaviour, it
  never blocks capture.
- macOS probe binaries missing / unparseable output: probe returns `None` →
  gate honestly skipped (existing contract), warn-once.
- Memory watermark breach mid-rung: `mem_high` gate failure → RollingBack →
  newest conns killed + expo hold — trips before the host is unusable.
- Weekend/off-hours: `max-smoke` exercises shard-cut of the 100K set + fleet
  machinery with tick gates skipped (SMOKE-labelled, never a live claim).
- Summary file unwritable: append is best-effort; the `info!` mirror + audit
  rows still carry the evidence.

## Failure Modes

- Provider rejects the scale-up (per-account cap): repeating GROWW-SCALE-02
  cooldown+halve — the honest discovery signal (unchanged).
- Rung failure: GROWW-SCALE-01 rollback to last healthy + expo hold
  (unchanged).
- Shard invariant violation: GROWW-SCALE-03 fail-closed halt (unchanged; the
  new builder's dedup preserves the cutter's precondition).
- Audit write failure: GROWW-SCALE-04 best-effort (unchanged).
- Host resource exhaustion: NEW `mem_high` + existing `cpu_high`/`disk_low`
  gates roll the fleet back automatically; Mac Ctrl-C +
  `make scale-test-clean` is the manual stop.
- Sidecar write IOPS saturation at high rungs: surfaces as capture lag /
  bridge-behind gate failures → rollback (existing gates).

## Test Plan

- `crates/common`: `test_groww_scale_full_master_and_mem_gate_defaults`
  (both new fields default OFF/85.0), `test_groww_scale_validate_rejects_bad_mem_gate`,
  TOML parse test for the new keys.
- `crates/core`: `test_full_master_entries_from_csv_builds_all_kinds`
  (IDX + CASH + FNO rows, kinds, ids), `test_full_master_entries_dedup_and_order`
  (cutter-order sort + duplicate-identity dedup),
  `test_full_master_entries_skips_non_numeric_non_idx`,
  `test_full_master_entries_empty_csv_fails`.
- `crates/app`: `test_gate_mem_high_and_unavailable_probe_skips`,
  smoke-gate test extension (memory stays real in smoke), pure probe parser
  tests (`parse_loadavg_pct`, `parse_meminfo_used_pct`,
  `parse_vm_stat_used_pct`), `test_format_stage_summary_row`.
- Harness: `DRY_RUN=1 bash scripts/groww-scale-test.sh max` +
  `... max-smoke` + `... clean` in the sandbox (overlay written/removed,
  no docker/cargo).
- `cargo fmt --check` + `cargo clippy --workspace --no-deps -- -D warnings`
  + `cargo test -p tickvault-common -p tickvault-core -p tickvault-app`
  (scoped; this leaf branch has no CI — local validation is the only gate).

## Rollback

- Config default-OFF: without the overlay the binary behaves byte-identically
  (single-conn Groww path; `full_master_universe=false`).
- `make scale-test-clean` removes the overlay block; `git branch -D
  local-runtime` deletes the entire lab (branch never merges to main).
- Mid-run: Ctrl-C stops the app; the ladder rehydrates the last
  VERIFIED-healthy rung on restart (unchanged).

## Observability

- Existing: `tv_groww_ladder_state` / `tv_groww_conns_desired` /
  `tv_groww_conns_target` gauges, `tv_groww_scale_rollbacks_total{reason}`
  (now also `reason="mem_high"`), `groww_scale_audit` rows per transition,
  per-conn panel `GET /api/feeds/health .groww_scale`, GROWW-SCALE-01..04.
- New: stage-summary TSV `data/groww-scale/summary-<date>.tsv` + `info!`
  mirror; full-master build logs (rows parsed / entries built / skipped /
  deduped / fallback-degrade).

## Plan Items

- [x] Item 1 — config: `full_master_universe` + `gate_max_mem_used_pct`
  - Files: crates/common/src/config.rs
  - Tests: test_groww_scale_full_master_and_mem_gate_defaults, test_groww_scale_validate_rejects_bad_mem_gate
- [x] Item 2 — full-master watch-set builder
  - Files: crates/core/src/feed/groww/instruments.rs
  - Tests: test_full_master_entries_from_csv_builds_all_kinds, test_full_master_entries_dedup_and_order, test_full_master_entries_skips_non_numeric_non_idx, test_full_master_entries_empty_csv_fails
- [x] Item 3 — ladder: mem gate + cross-platform probes + full-master path + summary emitter
  - Files: crates/app/src/groww_scale_ladder.rs
  - Tests: test_gate_mem_high_and_unavailable_probe_skips, test_parse_loadavg_pct, test_parse_meminfo_used_pct, test_parse_vm_stat_used_pct, test_format_stage_summary_row
- [x] Item 4 — harness: max / max-smoke modes + DRY_RUN + Makefile targets
  - Files: scripts/groww-scale-test.sh, Makefile
  - Tests: DRY_RUN=1 sandbox run (evidence in report)
- [x] Item 5 — runbook Monday sequence + max-scale section (purely Groww)
  - Files: docs/runbooks/groww-scale-test.md
  - Tests: n/a (docs)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `make scale-max` Monday, healthy | ladder climbs 1→2→5→10→20→40→80→100, halts at ceiling; summary TSV rows per stage |
| 2 | Master only has 62K usable rows | ceiling = 62 shards; ladder halts at 62 conns; actual recorded |
| 3 | Groww per-account cap < stage target | repeating GROWW-SCALE-02 halve — discovery verdict |
| 4 | Mac memory > 85% at rung 40 | `mem_high` rollback to last healthy before host is unusable |
| 5 | Full-master fetch fails 3× | loud fallback to ~767 activation set (degraded Tier-A run) |
| 6 | Weekend `make scale-max-smoke` | 100K shard-cut + fleet machinery validated, SMOKE-labelled |
