# Implementation Plan: NTM subscription PROOF harness (F1) + secid-46→443 comment reconcile + market-open self-test (F2)

**Status:** VERIFIED
**Date:** 2026-06-06
**Approved by:** Parthiban — "do both dude okay?" (run F1 live-proof + write the F1+F2 plan) +
"clearly tell me what is the security id for ntm" / live brutex master shows NTM index secid = 443.
**Crate(s) touched:** `core` (F1: `crates/core/examples/ntm_subscription_proof.rs`; Item 3:
2 stale comments in `crates/core/src/instrument/index_extractor.rs`; F2 FUTURE: a market-open
self-test sub-check in `crates/core/src/instrument/`).

## Context

Operator asked: are the **33 index values** (32 NSE allowlist incl. NIFTY TOTAL MKT + 1 BSE SENSEX)
AND the **~748 NIFTY Total Market constituent stocks** actually fetched / resolved / subscribed? The
§31 chain is merged (#1034–#1045) + `daily_universe_fetcher` is default-on. Open box was **live
runtime proof**. F1 closes it as far as the sandbox allows; F2 makes it self-policing. Separately the
operator's live master shows the NTM index secid is **443**, not the **46** that two tickvault
COMMENTS claim (46 was the Dhan web-chart IDX-segment value). tickvault reads the secid DYNAMICALLY
(match by SYMBOL_NAME "NIFTY TOTAL MKT"), so runtime is already correct — only the comments are
stale. Item 3 reconciles them so no future reader is misled.

## Design

- **F1 (this PR):** `crates/core/examples/ntm_subscription_proof.rs` runs the REAL pipeline —
  `parse_constituents` → `IndexConstituencyMap` → `resolve_constituents` — against the operator's
  REAL `ind_niftytotalmarket_list.csv`. HONEST ENVELOPE: true network end-to-end needs egress + a
  token; sandbox egress = 403 + QuestDB offline, so synthetic Dhan security_ids over the REAL ISINs
  (clearly labelled) prove the JOIN matches every real ISIN. Real ids come from Dhan's master at boot.
- **Item 3 (this PR):** edit the two `secid 46` comments in `index_extractor.rs` to state the secid is
  READ DYNAMICALLY from the live master (operator's live master = 443; 46 was the Dhan-chart IDX
  view). No logic change — tickvault never used 46 as a value.
- **F2 (FUTURE PR):** a pure-function market-open self-test sub-check asserting the live
  `subscription_targets` contains the 33 index values + the NTM constituent set (count ≥ floor),
  emitting an existing-style typed-code Telegram on shortfall. Pure logic + wiring + unit tests.

## Edge Cases

- CSV path missing → harness `expect` fails loudly (proof tool, not prod).
- niftyindices non-EQ series rows → parser drops them (measured: 7 dropped, 748 kept).
- Renamed ticker (symbol changed, ISIN same) → still resolves via ISIN (STEP 6 asserts true).
- Item 3: if the live master ever renames NTM, `allowlist_misses` boot telemetry already LOUD-warns —
  the comment fix notes this so the secid is never assumed.
- F2: NTM source degraded (`NTM-CONSTITUENCY-01`) → self-test treats core-universe as the floor.

## Failure Modes

- F1 is read-only/offline — cannot affect boot, feed, or any table.
- Item 3 is comments only — zero runtime effect.
- F2 (future) ALERTS on shortfall, never halts the feed; market-hours gated + edge-triggered.

## Test Plan

- F1: executed this session against the real file — **748 parsed (0 missing ISIN, 7 non-EQ dropped),
  748/748 resolved (100%) via ISIN, rename-proof = true**. Chain unit tests green:
  `constituent_resolver` 8, `daily_universe` 37, `index_extractor` 19, `index_constituency` 27 = 91.
- Item 3: `cargo test -p tickvault-core --lib instrument::index_extractor` stays green (comments only;
  `allowlist_has_exactly_32_nse_indices` unaffected).
- F2 (future): unit tests on the pure sub-check. `cargo test -p tickvault-core`.

## Rollback

- F1: single `examples/` file — `git revert` removes the proof tool; nothing references it.
- Item 3: comment-only — `git revert` restores prior text; no behavior either way.
- F2 (future): self-test sub-check + wiring — `git revert` removes the check; feed untouched.

## Observability

- F1 prints measured counts to stdout (the proof artifact). No new prod telemetry.
- Item 3: none (comments).
- F2 (future): reuses the 7-layer pattern (typed `ErrorCode` + runbook + Telegram + counter).

## Plan Items

- [x] Item 1 — F1 proof harness: real parse → map → resolve on the real NTM CSV + rename-proof
  - Files: `crates/core/examples/ntm_subscription_proof.rs`
  - Tests: resolve_constituents_by_isin_primary, isin_match_ignores_dhan_symbol_rename, parses_basic_two_row_csv
- [x] Item 3 — reconcile the 2 stale `secid 46` comments → note dynamic read (live master = 443)
  - Files: `crates/core/src/instrument/index_extractor.rs`
  - Tests: allowlist_has_exactly_32_nse_indices

> **FUTURE (F2, separate PR — NOT part of this PR's checklist):** a pure-function market-open
> self-test sub-check asserting the live `subscription_targets` contains the 33 index values + the
> NTM constituent set (count ≥ floor), emitting a typed-code Telegram on shortfall. Tracked here so
> it is not lost; it ships under its own approved plan.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Run F1 on the real NTM CSV | 748 parsed, 748/748 resolved via ISIN, rename-proof true |
| 2 | Live master NTM secid = 443 | tickvault reads 443 dynamically (never used 46); comments now match |
| 3 | niftyindices down at boot | core universe subscribed; `NTM-CONSTITUENCY-01` pages |
| 4 | F2 (future): an index value missing from live subscription | self-test fails + Telegram, feed runs |
