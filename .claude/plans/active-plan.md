# Implementation Plan: NTM Sub-PR #3a — lock the ISIN-primary mapping contract

**Status:** APPROVED
**Date:** 2026-06-06
**Approved by:** Parthiban — AskUserQuestion 2026-06-06 "Lock ISIN contract, then Sub-PR #3".
**Crate(s) touched:** none — docs/rule only (`daily-universe-scope-expansion-2026-05-27.md` §31.1).

## Context

Sub-PR #2 (cap 400→1200) merged as #1035. Before writing the constituency
downloader/mapping code (Sub-PR #3/#4), the operator asked to lock the precise
constituent → Dhan `security_id` mapping contract so the code builds to a stable,
fail-closed spec. This docs-only PR records that contract as rule §31.1.

## Plan Items

- [x] Item 1 — Add §31.1 "Constituent → Dhan security_id mapping contract"
  - Files: `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`,
    `.claude/plans/active-plan.md`
  - Tests: none (docs); additive section, no pinned guard string changes

## Design

§31.1 specifies: ISIN as the PRIMARY join key (rename/series-proof), filtered to
`EXCH_ID=NSE AND SEGMENT=E AND SERIES=EQ`; `(Symbol, Series=EQ)` as cross-check /
fallback (symbol-alone BANNED as primary); O(1) `HashMap<ISIN,(sid,segment)>`
build; fail-closed on unresolved ISIN (>0.5% dangling ⇒ reject build); union dedup
by `(security_id, exchange_segment)` (I-P1-11); role tagging; and the NTM index
value resolved separately from the master `SYMBOL_NAME` (not the constituent CSV).
Notes the one parser addition Sub-PR #3 makes: `CsvRow.isin` via `find("ISIN")`.

## Edge Cases

- Purely additive rule section → no `daily_universe_scope_guard` /
  `boot_step_timeout_constants_guard` pinned string changes → guards stay green.
- A constituent without an ISIN → §31.1 fallback (symbol+series) covers it.

## Failure Modes

- Docs-only; cannot affect build or runtime. Worst case: a typo in the contract.

## Test Plan

- `cargo test -p tickvault-storage --test daily_universe_scope_guard` stays green
  (no pinned string changed).
- `cargo build --workspace` unaffected (no code change).

## Rollback

- `git revert` removes §31.1 + this plan. Zero runtime impact.

## Observability

- N/A — docs only. The mapping's runtime observability (unresolved-ISIN counter,
  fail-closed reject error code, role counters) is specified for Sub-PR #4/#6.

## Per-Item Guarantee Matrix

| Demand | Sub-PR #3a |
|---|---|
| 100% testing | N/A (docs); Sub-PR #4 carries the mapping tests (ISIN resolve, dangling reject, union dedup, role) |
| O(1)/perf | contract mandates O(1) HashMap<ISIN→sid> lookup; measured in Sub-PR #4 |
| zero-loss/WS/QuestDB | unchanged — still 2 WS, Quote mode, DEDUP keys |
| uniqueness/dedup | contract pins `(security_id, exchange_segment)` union dedup (I-P1-11) |
| honest envelope | ISIN-primary is the precise key; symbol-alone explicitly banned; fail-closed on unresolved |
| scenarios | unresolved-ISIN / missing-ISIN-fallback / dangling-reject each become a Sub-PR #4 test |
