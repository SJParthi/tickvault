# Implementation Plan: O(1) per-feed uniqueness/dedup/mapping — permanent SoT doc + ratchet

**Status:** VERIFIED (operator 2026-06-24: "I need this always … persist … as a committed docs/architecture reference + ratchet … deep research … no illusion")
**Date:** 2026-06-24
**Branch:** `claude/o1-per-feed-architecture-doc` (off origin/main).

## Design
Create `docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md` as the PERMANENT
single source of truth for how the system holds O(1) per-feed uniqueness +
deduplication + mapping + latency across frontend/backend/db — GROUNDED in
cite-verified code (3 parallel verification agents), with the HONEST envelope
(O(1) time per op + O(1) per-key overhead; **bounded O(N×F) SPACE — NOT O(1)
space**; "100% inside the tested envelope" wording per charter §F).

Add a ratchet so the doc cannot silently drift from code:
`crates/storage/tests/o1_per_feed_doc_guard.rs` asserts the doc EXISTS and that
its core invariant claims still match the live code constants (the 4 feed-keyed
DEDUP keys all contain `feed`; the doc names the canonical composite key; the
doc carries the honest-envelope qualifier and does NOT claim "O(1) space").

## Plan Items
- [x] **G1 — write the SoT doc** `docs/architecture/o1-per-feed-uniqueness-dedup-mapping.md`
  - Synthesized ONLY from the 3 agents' verified findings (every claim cite-backed); honest-gaps section; honest-envelope wording; auto-driver explanation; comparison tables.
- [x] **G2 — ratchet test** `crates/storage/tests/o1_per_feed_doc_guard.rs`
  - doc exists; references each feed-keyed DEDUP const; each of those consts contains `feed`; doc contains the honest-envelope phrase; doc does NOT contain a literal "O(1) space" claim (anti-hallucination pin).
- [x] **G3 — cross-link** the doc from `data-integrity.md` (per-feed identity section) so future sessions find it.

## Edge Cases
- A future edit removes `feed` from a DEDUP key → both the existing dedup meta-guard AND this doc-guard fail.
- A future edit weakens the doc to claim "O(1) space" → doc-guard fails (anti-hallucination).
- Doc renamed/moved → guard fails (path pinned).

## Failure Modes
- Doc drifts from code → G2 guard fails the build.
- Agent finding contradicts a claim → that claim is DROPPED or marked honest-gap (no aspirational text ships).

## Test Plan
- `cargo test -p tickvault-storage --test o1_per_feed_doc_guard`
- `cargo test -p tickvault-storage --test dedup_segment_meta_guard` (still green)
- `cargo fmt --check` + `cargo clippy -p tickvault-storage -- -D warnings`
- 3-agent adversarial review on the diff (operator demanded full panel).

## Rollback
- Docs + one test file; `git revert`. No runtime code touched.

## Observability
- N/A (doc + test). The doc itself documents the 7-layer observability already in place.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | doc present + claims match code | guard green |
| 2 | someone removes `feed` from a key | dedup guard + doc guard both fail |
| 3 | someone writes "O(1) space" in the doc | doc guard fails (honesty pin) |
