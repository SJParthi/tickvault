# Implementation Plan: NTM Sub-PR #10 — turn ON real NTM data flow at boot

**Status:** APPROVED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06: niftyindices failure policy = **Degrade + alert**
(proceed on the indices + F&O-underlyings core universe + Critical alert; Dhan CSV stays fail-closed
§4) + standing "once merged, go ahead with the plan".
**Crate(s) touched:** `core` (`daily_universe_orchestrator.rs`, new bridge fn; `index_constituency/`
glue), `app` (`daily_universe_boot.rs`/`main.rs` async niftyindices fetch + degrade wiring),
`common` (`error_code.rs` new `NtmConstituency01SourceDegraded` + rule-file mention),
`core/notification` (new `NotificationEvent` variant). Feature: `daily_universe_fetcher`.

## Context

This is the **live turn-on** of the §31 NTM subscription. Sub-PRs #2–#5 built every piece behind
the feature flag, passing `ntm_constituents = Vec::new()` so live boot stayed unchanged. #10 wires
the real data so the ~750-stock NTM union actually flows on the single main-feed WS.

## The seam (verified against merged code)

```
boot (app, async)                         orchestrator (core, sync)
─────────────────                         ─────────────────────────
csv_downloader::fetch_csv() ── dhan_bytes ─┐
ConstituencyDownloader::fetch_slug(        │
  "ind_niftytotalmarket_list")             │
  → Result<Vec<u8>>  [DEGRADE on Err]──ntm_bytes(Option)
                                           ▼
        build_universe_from_bytes(dhan_bytes, ntm_bytes: Option<&[u8]>)
           parse_detailed_csv(dhan_bytes) → rows
           extract_indices / extract_fno_underlyings / collect_fno_contracts
           IF ntm_bytes:
             parse_constituents(ntm,"Nifty Total Market",today) → ParsedConstituents
             assemble IndexConstituencyMap
             resolve_constituents(map, &rows) → ResolveOutcome   [DEGRADE on TooManyDangling]
             BRIDGE: HashMap<(sid,seg),&CsvRow> over rows; map each resolved → cloned CsvRow
                     → ntm_constituent_rows: Vec<CsvRow>
           build_daily_universe(indices, fno, fno_contracts, ntm_constituent_rows)
```

## Design

- **Bridge fn** (core, e.g. `daily_universe_orchestrator::resolve_ntm_rows(rows: &[CsvRow], ntm_bytes: &[u8], today) -> Result<Vec<CsvRow>, NtmDegradeReason>`):
  parse → assemble map → `resolve_constituents` → build `HashMap<(security_id,segment), &CsvRow>`
  (O(N) once) → O(1) lookup per resolved → clone → `Vec<CsvRow>`. Every resolved SID is sourced
  FROM `rows`, so the lookup never misses; a miss is counted, never panics.
- **Orchestrator**: `build_universe_from_bytes` gains `ntm_bytes: Option<&[u8]>`. On any NTM-side
  error (parse / `TooManyDangling` / empty) → **degrade**: log `error!(code=NTM-CONSTITUENCY-01)`,
  treat NTM as empty, continue building the core universe (NOT an `OrchestratorError`). The Dhan
  core path stays fail-closed exactly as today.
- **Boot caller** (app): async `ConstituencyDownloader::fetch_slug("ind_niftytotalmarket_list")`;
  on `Err` → `None` + degrade alert. Pass `ntm_bytes.as_deref()` to the orchestrator. (Cache-fallback
  was NOT selected by the operator — pure degrade.)
- **New `ErrorCode::NtmConstituency01SourceDegraded`** (`NTM-CONSTITUENCY-01`, Severity::Critical,
  auto-triage NO) + rule-file `.claude/rules/project/ntm-constituency-error-codes.md` (cross-ref test).
- **New `NotificationEvent::NtmConstituencyDegraded { reason, core_universe_size }`** (Critical) so
  the operator is paged that today runs without the ~500 cash-only constituents.

## Edge Cases

- niftyindices 5xx / timeout / DNS fail → degrade, core universe lives, Critical alert.
- niftyindices 200 but malformed / non-UTF-8 / missing column → degrade (parse error).
- `> 0.5%` constituents dangling vs Dhan rows → `resolve_constituents` `TooManyDangling` → degrade.
- All constituents already F&O underlyings → universe size unchanged; both-flags set (no dup).
- NTM pushes universe past `MAX_DAILY_UNIVERSE_SIZE = 1200` → `build_daily_universe` envelope reject
  (fail-closed — this is a Dhan/NTM data anomaly worth halting on, NOT a degrade).
- Weekend/holiday boot → existing holiday gate; niftyindices fetch best-effort.

## Failure Modes

- NTM source down → DEGRADE + `NTM-CONSTITUENCY-01` Critical (operator-chosen). Core trading continues.
- Bridge lookup miss (should be impossible) → counted, skipped, never panics.
- Envelope breach after fold → `INSTR-FETCH-04` fail-closed HALT (unchanged).
- Dhan CSV failure → §4 infinite-retry fail-closed (unchanged — NTM policy does NOT relax this).

## Test Plan

`cargo test -p tickvault-core --features daily_universe_fetcher` + scoped app + workspace test-compile
(disk: scoped only — full workspace link exceeds the container; CI is the full-workspace gate).

- bridge: resolved→CsvRow mapping correct; both-case dedup; pure-constituent added; lookup-miss skip.
- orchestrator degrade: malformed ntm_bytes → core universe built, NTM empty, code emitted (not Err).
- orchestrator success: valid ntm_bytes → universe grows by the resolved count; counts correct.
- envelope: NTM over-MAX → `INSTR-FETCH-04` (still fail-closed).
- ErrorCode: variant + `code_str` + rule-file cross-ref + tag-guard.
- NotificationEvent severity Critical.

## Rollback

NTM is degrade-by-design: passing `ntm_bytes = None` (or feature off) restores the exact pre-#10
core universe. `git revert` clean; no migration. The live turn-on is gated by the niftyindices fetch
succeeding — a revert or a source outage both fall back to the proven core universe.

## Observability

`NTM-CONSTITUENCY-01` Critical → Telegram + `errors.jsonl`; boot `info!` adds the resolved NTM count
+ `tv_universe_size{kind="index_constituent"}` (already added in #5) now non-zero on success. Audit
table additions deferred to a follow-up (the lifecycle master already records roles via #9/#5).

## Plan Items

- [ ] Item 1 — `ErrorCode::NtmConstituency01SourceDegraded` + rule file (cross-ref + tag-guard)
  - Files: `crates/common/src/error_code.rs`, `.claude/rules/project/ntm-constituency-error-codes.md`
- [ ] Item 2 — `NotificationEvent::NtmConstituencyDegraded` (Critical)
  - Files: `crates/core/src/notification/events.rs`
- [ ] Item 3 — bridge fn + orchestrator `ntm_bytes: Option<&[u8]>` + degrade
  - Files: `crates/core/src/instrument/daily_universe_orchestrator.rs`
  - Tests: bridge mapping, degrade-on-malformed, success-grows-universe, envelope-breach-halts
- [ ] Item 4 — boot caller async niftyindices fetch + degrade alert wiring
  - Files: `crates/app/src/daily_universe_boot.rs` (and/or `main.rs`)
  - Tests: existing boot suites recompile + pass; degrade-path source-scan guard
- [ ] Item 5 — 3-agent adversarial review (hot-path/security/hostile) before+after; fix CRITICAL/HIGH

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | niftyindices healthy | universe = indices + F&O underlyings + ~500 cash NTM; both-flags on overlap |
| 2 | niftyindices down | core universe only + `NTM-CONSTITUENCY-01` Critical; trading continues |
| 3 | malformed NTM CSV | degrade (not a boot halt); Dhan core unaffected |
| 4 | NTM over MAX=1200 | `INSTR-FETCH-04` fail-closed HALT |
| 5 | revert / feature off | exact pre-#10 core universe |
