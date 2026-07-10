# Implementation Plan: FULL removal of the option_chain REST subsystem

**Status:** APPROVED
**Date:** 2026-06-28
**Approved by:** Parthiban — operator directive 2026-06-28 (verbatim): *"i clearly
asked you to drop the option chain entire implementations and its table also"*.

## Authority / supersession

This directive SUPERSEDES the 2026-06-23 VERIFIED decision
`active-plan-drop-option-chain-snapshot-table.md` (Status: VERIFIED), which dropped
ONLY the `option_chain_minute_snapshot` QuestDB table and explicitly KEPT the
scheduler structure, the REST client, the `OptionChain01-08` error codes, the
config, and the notifications ("Minimal scope (operator-locked): remove the QuestDB
TABLE — its creation + its write-path — and KEEP everything else"). The 2026-06-28
operator directive now removes the **entire** subsystem.

The option_chain REST fetcher has been **disabled since 2026-06-02**
(`config/base.toml [option_chain_minute_snapshot] enabled = false` — the account
lacks the Option Chain Data API entitlement) with **ZERO production consumer of its
output cache**. The boot block at `main.rs` ~6497-6612 only spawns the scheduler when
`enabled == true`, which it never is. The orphaned prev_oi option-chain overlay at
`main.rs` ~5272-5312 builds a `prev_oi_cache` that is immediately shadowed by the
tick-enricher's own `prev_day_ohlcv`-sourced load at ~5360-5391.

This removal is also consistent with `no-rest-except-live-feed-2026-06-27.md` §3,
which already marks `POST /v2/optionchain` (`option_chain_cache_loader.rs`) as
**REMOVE**.

## Scope boundary (precise — KEEP vs REMOVE)

REMOVE (the disabled REST subsystem + its exclusive dependencies):
- `crates/core/src/option_chain/` (whole module: client, snapshot_cache,
  snapshot_scheduler, current_expiry_cache, expiry_warmup, l2_verify, prev_oi,
  burst_orchestrator, types, mod) + `pub mod option_chain;` in `core/src/lib.rs`.
- `crates/app/src/option_chain_cache_loader.rs` + `pub mod` in `app/src/lib.rs`.
- `crates/app/src/prev_oi_loader.rs` (its ONLY public fn
  `load_prev_oi_cache_at_boot_with_overlay` is the option-chain overlay; its single
  call site is the orphan block in `main.rs`) + `pub mod prev_oi_loader;` in
  `app/src/lib.rs`.
- `crates/common/src/option_chain_schedule.rs` (the `[option_chain_minute_snapshot]`
  validator) + `pub mod option_chain_schedule;` in `common/src/lib.rs`.
- `config.rs`: `OptionChainMinuteSnapshotConfig` + `OptionChainUnderlyingEntry`
  structs + the `option_chain_minute_snapshot` field + its `..Default` wiring.
- `config/base.toml`: the `[option_chain_minute_snapshot]` block AND the
  `[option_chain]` doc-stub block (both are option_chain-only; `[option_chain]` has
  NO struct binding — verified).
- `constants.rs`: the 9 `OPTION_CHAIN_*` constants + their pinning unit tests.
- `error_code.rs`: `OptionChain01..08` variants (8) + their `code_str()` / `all()` /
  severity / runbook arms.
- `events.rs`: 4 variants `OptionChainFetchFailed`, `OptionChainCacheFallback`,
  `OptionChainStaleHalt`, `OptionChainConfigInvalid` + their Telegram formatting +
  `event_name()` + `severity()` arms.
- `locked_universe.rs`: `OPTION_CHAIN_UNDERLYINGS`, `OPTION_CHAIN_FETCH_SEQUENCE`,
  `has_option_chain()` + their unit tests — these serve ONLY the REST subsystem
  (consumed exclusively by the deleted `expiry_warmup.rs` + the deleted
  `option_chain_warmup_wiring.rs` test). They are NOT live-subscription state, so
  removing them does NOT touch the 2-WS lock / daily-universe contract.
- `observability.rs`: `LogCategory::OptionChain` variant + `CATEGORY_OPTION_CHAIN_*`
  consts + `all()` 3→2 + `build_category_targets` arm + pinning tests.
- `secret_manager.rs`: `test_option_chain_minute_snapshot_scheduler_is_wired_into_main`
  source-scan wiring guard (asserts deleted code is present — must be removed).
- main.rs: the minute-snapshot scheduler boot block (~6497-6612) + the orphan
  prev_oi overlay block (~5272-5312, replace with a plain `slow_registry`-absent
  warn so the `else` branch + downstream stays intact) + stale `option_chain` DDL
  comment refs (~4549, ~74).
- Tests: delete `crates/common/tests/option_chain_warmup_wiring.rs` +
  `crates/common/tests/intra_minute_retry_guard.rs` (both 100% option_chain);
  surgically remove the option_chain test/section from `dhan_api_coverage.rs`,
  `price_precision_wiring.rs`, `runbook_cross_link_guard.rs`,
  `event_formatting_coverage.rs`.
- Rule files: flip `no-rest-except-live-feed-2026-06-27.md` §3 optionchain row to
  REMOVED/done; mark `option-chain-cross-verify-error-codes.md` §1 codes RETIRED;
  update `observability-architecture.md` ErrorCode count + category count;
  mark `.claude/rules/dhan/option-chain.md` module-retired.

KEEP (out of scope — universe data model + live OI-change path):
- `instrument_types.rs` `OptionChain` / `OptionChainKey` / `OptionChainEntry` /
  `option_chains` field on the universe — these are the F&O universe data model used
  by live subscription planning, NOT the REST subsystem.
- `subscription_planner.rs` `option_chain_lookup` / `OptionChainKey` usage — live
  subscription planning.
- The live OI-change feature: the tick enricher's `prev_oi_cache` loaded from
  `prev_day_ohlcv` (main.rs ~5360-5391) + `spawn_prev_oi_cache_refresh_task` — MUST
  keep compiling + working.
- `prev_day_ohlcv_boot.rs`, `prev_day_ohlcv_persistence` — separate, kept.
- `PrevOi01CacheEmptyAtBoot` ErrorCode — verify: its only emit site is the orphan
  overlay block being removed; if so it loses its emitter. Decision: KEEP the
  variant (it is also mentioned in wave-1-error-codes.md and the cross-ref guard
  requires variant↔rule sync; removing it would require editing wave-1 too).
  Removing the emit site is fine — an ErrorCode with no current emitter is allowed
  (e.g. PrevClose02 is explicitly "reserved, not emitted"). The orphan block's warn
  is deleted; PrevOi01 stays defined + rule-documented.

## Plan Items

- [x] Item 1 — delete `crates/core/src/option_chain/` + `pub mod` in core/lib.rs
  - Files: crates/core/src/option_chain/* (10 files), crates/core/src/lib.rs
  - Tests: scoped `cargo check -p tickvault-core`

- [x] Item 2 — delete app option_chain modules + prev_oi overlay
  - Files: crates/app/src/option_chain_cache_loader.rs,
    crates/app/src/prev_oi_loader.rs, crates/app/src/lib.rs (2 `pub mod`)
  - Tests: scoped `cargo check -p tickvault-app`

- [x] Item 3 — main.rs boot de-wiring (minute-snapshot block + orphan prev_oi
    overlay block + comment refs), KEEP live `prev_day_ohlcv` enricher path
  - Files: crates/app/src/main.rs
  - Tests: scoped `cargo check -p tickvault-app`; OI-change path still compiles

- [x] Item 4 — common: delete option_chain_schedule.rs, config structs +
    field, constants + tests
  - Files: crates/common/src/option_chain_schedule.rs, crates/common/src/lib.rs,
    crates/common/src/config.rs, crates/common/src/constants.rs
  - Tests: `cargo check -p tickvault-common`, `cargo test -p tickvault-common --lib`

- [x] Item 5 — common: error_code.rs OptionChain01..08 removal (variants + all arms)
  - Files: crates/common/src/error_code.rs
  - Tests: error_code internal unit tests (`test_all_variants_*`, `test_all_list_length_*`)

- [x] Item 6 — locked_universe.rs OPTION_CHAIN_* constants + tests removal
  - Files: crates/common/src/locked_universe.rs
  - Tests: `cargo test -p tickvault-common --lib locked_universe`

- [x] Item 7 — config/base.toml block removal (both option_chain blocks)
  - Files: config/base.toml
  - Tests: `cargo test -p tickvault-common` (config load test)

- [x] Item 8 — core: events.rs 4 OptionChain* variants removal (def + format +
    name + severity arms)
  - Files: crates/core/src/notification/events.rs
  - Tests: `cargo check -p tickvault-core`

- [x] Item 9 — observability.rs LogCategory::OptionChain removal (3→2) + tests
  - Files: crates/app/src/observability.rs
  - Tests: observability unit tests (`test_all_*`, dir/prefix asserts)

- [x] Item 10 — secret_manager.rs wiring-guard test removal
  - Files: crates/core/src/auth/secret_manager.rs
  - Tests: `cargo check -p tickvault-core`

- [x] Item 11 — test files: delete 2 wholesale, surgically edit 4
  - Files: crates/common/tests/option_chain_warmup_wiring.rs (delete),
    crates/common/tests/intra_minute_retry_guard.rs (delete),
    crates/common/tests/dhan_api_coverage.rs (edit),
    crates/common/tests/price_precision_wiring.rs (edit),
    crates/common/tests/runbook_cross_link_guard.rs (edit),
    crates/core/tests/event_formatting_coverage.rs (edit)
  - Tests: `cargo test -p tickvault-common`, `cargo check -p tickvault-core --tests`

- [x] Item 12 — rule-file sync (CRITICAL: error_code_rule_file_crossref keeps
    variants ↔ rule mentions in step). Flip no-rest §3 row; retire option-chain
    codes; update observability-architecture counts; mark dhan/option-chain.md retired
  - Files: .claude/rules/project/no-rest-except-live-feed-2026-06-27.md,
    .claude/rules/project/option-chain-cross-verify-error-codes.md,
    .claude/rules/project/observability-architecture.md,
    .claude/rules/dhan/option-chain.md
  - Tests: `cargo test -p tickvault-common --test error_code_rule_file_crossref`

## Design

The option_chain subsystem is a self-contained REST fetcher (core module +
app boot wiring + common config/constants/error-codes + observability category)
that is config-gated OFF and has no live consumer. Removal is a vertical deletion
of that subsystem only, leaving (a) the universe `OptionChain*` data-model types in
`instrument_types.rs`/`subscription_planner.rs` and (b) the live OI-change path that
reads `prev_day_ohlcv` fully intact. The compiler is the precision tool: after each
crate's deletion a scoped `cargo check -p` surfaces every remaining reference, so no
dangling import survives. The dependency order is leaf-first (core module → app
wiring → common contracts) to minimise intermediate breakage. The ErrorCode +
rule-file edits land in the SAME change-set (Item 5 + Item 12) so the bidirectional
`error_code_rule_file_crossref` guard (variant→rule AND rule→variant) stays green.

## Edge Cases

- OI-change feature must still compile + work: it reads `prev_oi_cache` from
  `prev_day_ohlcv` via the tick enricher (main.rs ~5360-5391) + the periodic
  `spawn_prev_oi_cache_refresh_task` — neither touches `option_chain`. Proven by
  `cargo check -p tickvault-app` after Item 3 + a grep showing no `option_chain` in
  the enricher path.
- `PrevOi01CacheEmptyAtBoot` ErrorCode loses its only emit site (the orphan overlay
  warn). Kept as a defined-but-unemitted variant (allowed pattern — PrevClose02 is
  the precedent) so wave-1-error-codes.md stays valid and the cross-ref guard passes.
- `error_code_rule_file_crossref` is bidirectional: deleting OptionChain01..08 from
  the enum REQUIRES deleting/retiring their mentions in the 2 rule files in the same
  change — done in Item 12. The retired-marker text keeps the human history without
  re-introducing a live `OPTION-CHAIN-0N` token that the rule→variant direction
  would flag (the guard matches the `OPTION-CHAIN-NN` code token; the rule files will
  state "RETIRED 2026-06-28" and remove the live code tokens, OR the guard's
  allowlist handles them — verify the guard's exact matching before finalising).
- `[option_chain]` (non-minute) toml stub has NO struct binding → removing it cannot
  break config deserialization (`deny_unknown_fields` is NOT set; verified by the
  base.toml comment itself).
- Two test files (`option_chain_warmup_wiring.rs`, `intra_minute_retry_guard.rs`) are
  100% option_chain → delete wholesale; deleting whole test files lowers the
  test-count ratchet — note this is expected for a removal PR (the ratchet is
  "count can only increase" per CLAUDE.md; a removal PR is the legitimate exception,
  flagged in the PR body).

## Failure Modes

- A missed reference → `cargo check -p <crate>` fails with E0433/E0599; fix before
  moving on. Scoped builds keep disk pressure low (no full workspace build).
- The test-count ratchet (pre-push gate 4) may block because whole test files are
  deleted. Mitigation: this is a removal PR; if the gate blocks, report to operator
  (do NOT bypass with --no-verify). The CI removal exception is the operator's call.
- `error_code_rule_file_crossref` fails if variant/rule edits drift → both edited in
  the same change-set + the dedicated test run in VERIFY.
- Disk fills mid-build → `df -h` checkpoint; STOP + report if <4G free.

## Test Plan

Scoped, disk-safe (no full `cargo build --workspace`):
- `cargo check -p tickvault-common -p tickvault-core -p tickvault-app`
- `cargo test -p tickvault-common --lib` (error_code internal guards, config load,
  locked_universe, constants, observability lives in app)
- `cargo test -p tickvault-common --test error_code_rule_file_crossref`
- `cargo test -p tickvault-common --test dhan_api_coverage`
- `cargo test -p tickvault-common --test price_precision_wiring`
- `cargo test -p tickvault-common --test runbook_cross_link_guard`
- `cargo test -p tickvault-core --test event_formatting_coverage`
- `cargo test -p tickvault-app --lib` (observability LogCategory tests)
- `cargo fmt`
- `rg -i option_chain crates/` shows only the KEPT universe-model refs
  (instrument_types/subscription_planner) — no REST-subsystem residue.

## Rollback

Single feature branch `claude/remove-option-chain` off origin/main. `git revert`
of the squash-merge restores the entire subsystem from history (every deleted file
is recoverable from git). No data migration is involved (the QuestDB table was
already dropped in PR #1227's predecessor 2026-06-23). No runtime state depends on
the removed code (it was disabled), so revert is behaviour-neutral.

## Observability

This removal DELETES observability surface (the `LogCategory::OptionChain` log
category, the 4 OptionChain Telegram events, the OPTION-CHAIN-0N error codes). No
new observability is added — the removed surface monitored a disabled subsystem with
no live consumer, so its deletion removes false-positive monitoring of dead code.
The live OI-change path keeps its existing `tv_prev_oi_cache_*` counters + PREVDAY-01
/ PREVCLOSE-04 codes untouched. Honest envelope: this is a pure-deletion change of a
disabled subsystem; no runtime behaviour of the live feed, dedup, tick pipeline,
2-WS lock, or OI-change feature changes.

## Per-Item Guarantee Matrix (cross-ref `.claude/rules/project/per-wave-guarantee-matrix.md`)

### 15-row "100% everything" matrix

| Demand | This PR's specifics |
|---|---|
| 100% code coverage | Pure deletion; remaining code's coverage unchanged. CI llvm-cov runs post-merge. |
| 100% audit coverage | N/A — removes a subsystem; the `option_chain_minute_snapshot` audit table was already dropped 2026-06-23. No audit table added/removed here. |
| 100% testing coverage | Scoped `cargo test -p` for common/core/app; deleted tests covered only deleted code. |
| 100% code checks | banned-pattern + plan-verify + plan-gate (this plan satisfies the 6 design-first sections) + fmt. |
| 100% code performance | N/A — no hot-path code added; deletion only. No DHAT/Criterion delta. |
| 100% monitoring | Removes monitoring of dead code (LogCategory::OptionChain). Live-path monitoring untouched. |
| 100% logging | 4 OptionChain `error!`/Telegram events removed (their subsystem is gone); no new log sites. |
| 100% alerting | OPTION-CHAIN-0N alert codes removed with their emitters; live-path alerts untouched. |
| 100% security | security-reviewer agent runs on the diff (sequential, post-impl). No new attack surface (deletion). |
| 100% security hardening | N/A — no new secret/IP/auth surface; removes a disabled REST client. |
| 100% bugs fixing | adversarial 3-agent review (hot-path + security + hostile) on `git diff origin/main`. |
| 100% scenarios covering | Edge cases enumerated above (OI-change survives, PrevOi01 reserved, toml stub, test-count ratchet). |
| 100% functionalities covering | Every removed `pub fn`'s call sites are removed in the same change (pub-fn-wiring guard). |
| 100% code review | 3-agent pass BEFORE (this design) + AFTER (on diff). |
| 100% extreme check | `cargo check` across 3 crates + the cross-ref ratchet + fmt fail the build on any residue. |

### 7-row "Resilience" matrix

| Demand | This PR's proof |
|---|---|
| Zero ticks lost | No tick-drop path touched; ring→spill→DLQ + WAL unchanged (deletion is downstream-of-feed, REST-only). |
| WS never disconnects | No WS code touched (option_chain is REST-only). SubscribeRxGuard + pool watchdog untouched. |
| Never slow/locked/hanged | No hot-path code added; deletion removes a disabled cold-path REST loop. |
| QuestDB never fails | No QuestDB writer added/removed (table already gone 2026-06-23); schema self-heal untouched. |
| O(1) latency | N/A — no lookup/dedup/hot-path logic changed. |
| Uniqueness + dedup | No DEDUP key touched; the kept universe-model `OptionChainKey` retains its composite identity. |
| Real-time proof | Scoped test output pasted in PR body; `rg -i option_chain crates/` residue check. |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the option_chain
REST subsystem (disabled since 2026-06-02, zero live consumer) is fully removed; the
live OI-change feature (reads `prev_day_ohlcv`, not option_chain) keeps compiling and
working, proven by `cargo check -p tickvault-app` + grep showing no option_chain in
the enricher path; the bidirectional `error_code_rule_file_crossref` guard stays green
because the 8 ErrorCode variants and their 2 rule-file mentions are edited in the same
change-set; the 2-WebSocket lock, daily-universe contract, tick pipeline, dedup, and
WAL/ring/spill/DLQ chain are entirely untouched (option_chain is REST-only and
downstream of the feed). Beyond the envelope: this is a pure deletion of dead code —
there is no runtime behaviour of the live system to regress.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | App boots with option_chain removed | identical behaviour (subsystem was disabled) |
| 2 | OI-change feature reads prev_day_ohlcv | works unchanged; no option_chain dependency |
| 3 | config loads without [option_chain*] blocks | success (no struct binding for the removed fields after Item 4) |
| 4 | `cargo test error_code_rule_file_crossref` | green (variants ↔ rules in sync) |
| 5 | `rg -i option_chain crates/` | only instrument_types/subscription_planner universe-model refs remain |
