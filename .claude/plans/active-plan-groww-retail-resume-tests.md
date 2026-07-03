# Implementation Plan: Groww bridge re-tail/resume recovery test suite (B2)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — standing autonomy directive this session, 2026-07-03 ("go ahead dude" + ultracode standing ON)

> **Scope:** TEST-ONLY. Real CI-runnable `#[tokio::test]`s for the Groww bridge
> re-tail/resume crash-recovery flow (`GrowwBridgeState::try_resume_from_snapshot`
> + `drain_new_data` byte-offset mechanics) in
> `crates/app/src/groww_bridge.rs` (crate: tickvault-app / `app`).
> ZERO production-logic changes.

## Why

The Groww lane has NO WAL — the NDJSON re-tail/resume IS its entire crash-recovery
mechanism, yet on origin/main (verified 2026-07-03 against the actual source):

- `try_resume_from_snapshot` (groww_bridge.rs:707) has **zero test callers** —
  only the pure `resume_from_snapshot` decision matrix is tested.
- The **archive-drain branch** (`should_drain_archive` → the wake-capped drain loop,
  groww_bridge.rs:749-793) is never executed by any test — only the pure boolean
  decision is.
- `drain_new_data`'s offset/shrink/residual mechanics have only a
  no-file no-op test (`test_drain_new_data_no_file_is_noop_fallback_safe`).
- The only e2e touching replay (`groww_live_pipeline_e2e`) requires a live QuestDB
  and skips in CI.

The just-merged chaos matrix (`crates/app/tests/chaos_feed_state_matrix.rs:24-31`)
leans on this re-tail as Groww's documented recovery story. This block converts the
source-pinned promise into executed proof: a regression = silent tick loss or a
permanent stall with green CI.

## Design

All tests live in the existing `mod tests` inside
`crates/app/src/groww_bridge.rs` (the surfaces are `pub(crate)`/private —
unreachable from the `tests/` dir; a child module has access, so **no testability
seam is needed**: zero non-test code lines).

- **No QuestDB dependency:** each test builds `GrowwBridgeState::with_aggregator`
  around a `QuestDbConfig` pointing at `127.0.0.1:1` (reserved/refused port — the
  exact `for_test()` fail-fast idiom from
  `crates/storage/src/groww_persistence.rs:216-225`). `GrowwLiveTickWriter::new`
  degrades to local buffering (groww_persistence.rs:198-208); every in-drain
  `flush()` fails fast after the bounded 50+100+200 ms reconnect ladder, so
  `pending()` RETAINS the exact count of rows appended — the observable that
  proves exact line counts (zero loss, zero duplicates).
- **Temp dirs:** pid + monotonic-nanos unique under `std::env::temp_dir()`,
  removed at test end (the `chaos_tmp` idiom).
- **IST-midnight race guard:** the resume tests key snapshots on the REAL current
  IST day (`ist_day_from_ist_nanos(receipt_ist_nanos())`, exactly what production
  computes). A test spanning IST midnight would flip the day mid-test; each
  resume test therefore runs inside a bounded (≤2 attempts) retry that re-runs
  with the new day if the day flipped — deterministic without any clock seam.
- **Tick lines:** valid NDJSON per the module contract, `ts_ist_nanos` in the
  past (~2026-05, inside the `[2020, 2100)` plausibility window, never ahead of
  the receipt clock), so `validate_groww_tick` accepts every line.

### Honest envelope (stated plainly, per operator-charter §F)

The snapshot-PERSIST arm (`maybe_persist_offset` inside the flush-`Ok` arm) is
**NOT executed** by these tests — CI has no QuestDB, so the flush never returns
`Ok`. The resume tests exercise the READ side by hand-writing the snapshot JSON
(the same `GrowwOffsetSnapshot` serde shape the persist arm writes, round-trip
pinned by the existing `test_offset_snapshot_roundtrip`). The persist-side
ordering is pinned by the existing source-scan ratchet
`test_offset_persist_is_after_flush_ok`; the live persist path remains covered
only by the QuestDB-gated e2e. This is a documented gap, not a claimed coverage.

## Edge Cases

Covered by the suite (each is a named test):

1. N complete NDJSON lines + trailing PARTIAL line → offset == file length,
   partial carried in `residual`, completed on the next drain — no line lost, none
   duplicated (exact `pending()` counts).
2. File SHRINK (rotation/truncate to a shorter file) → offset resets to byte 0,
   the new file is drained — no permanent stall (subsequent appends keep flowing).
3. Mid-file resume: hand-written same-day snapshot at a line boundary → outcome
   "resumed": `offset` == snapshot offset, `capture_seq` restored, drain parses
   ONLY the tail lines (exact count ⇒ zero double-read).
4. Cross-day snapshot + archive present → the `should_drain_archive` branch
   EXECUTES: archive tail (beyond the flushed offset) drained fully through the
   real parse/validate/append path, then byte-0 re-tail of today's live file;
   `active_drain_ist_day` cleared, residual cleared.
5. Corrupt snapshot JSON → fail-closed byte-0 full re-tail.
6. Wrong-day snapshot with NO archive on disk → fail-closed byte-0 full re-tail.
7. Snapshot offset beyond the (truncated) file length, same day + matching head →
   fail-closed byte-0 full re-tail (never a partial/undefined resume).
8. Snapshot pointing at a capture file that no longer exists → snapshot ignored,
   offset stays 0, drain of the missing file is a safe no-op.
9. Empty capture file → no-op drain, then a first appended line is picked up.

## Failure Modes

What a regression in the covered mechanics would have meant (and what now fails):

- Offset advanced past unread bytes / residual dropped → **silent tick loss** →
  exact-count assertions (1) fail.
- Shrink not detected (offset > len left in place) → **permanent stall** (drain
  forever returns "nothing new") → test (2) fails.
- Resume accepting a snapshot it shouldn't (corrupt/cross-day/over-length) →
  **skipped prefix = silent loss** → tests (5)(6)(7) fail.
- Resume re-reading from byte 0 despite a valid snapshot → **duplicate replay
  pressure** (DEDUP absorbs it in prod, but the resume contract is broken) →
  test (3)'s exact tail count fails.
- Archive branch skipped or draining from byte 0 → **orphaned pre-rotation tail**
  or duplicated archive prefix → test (4)'s exact counts fail.

Test-infra failure modes: unreachable-port flush adds ≤ ~350 ms per drain call
(bounded ladder, never hangs); IST-midnight flip mid-test → bounded re-run;
temp-dir collisions prevented by pid+nanos naming.

## Test Plan

- [x] Suite in `crates/app/src/groww_bridge.rs` `mod tests`
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_retail_drain_offset_tracks_complete_lines_and_carries_partial,
    test_retail_drain_file_shrink_resets_offset_no_stall,
    test_retail_resume_mid_file_no_double_read,
    test_retail_cross_day_snapshot_drains_archive_then_byte0_retail,
    test_retail_corrupt_snapshot_fails_closed_to_byte0,
    test_retail_wrong_day_snapshot_without_archive_fails_closed_to_byte0,
    test_retail_snapshot_offset_beyond_truncated_file_fails_closed_to_byte0,
    test_retail_snapshot_for_missing_file_is_ignored,
    test_retail_drain_empty_file_noop_then_first_line_flows
- [x] Verification battery: `cargo fmt --all` + `--check`; scoped
  `cargo test -p tickvault-app groww_bridge` + full `cargo test -p tickvault-app`;
  `cargo clippy --workspace --no-deps` (CI's exact invocation — ci.yml:160);
  banned-pattern-scanner; plan-verify; per-item-guarantee-check; plan-gate.
- [x] NEGATIVE proof: sabotage the mid-file resume assertion to expect a
  duplicate re-read (pending == 5 instead of 2), observe the suite FAIL, revert.

### Verification outputs (pasted after execution)

```
cargo fmt --all && cargo fmt --all --check   → clean (exit 0)

cargo test -p tickvault-app --lib groww_bridge   (scoped filter)
test result: ok. 76 passed; 0 failed; 0 ignored; 0 measured; 686 filtered out; finished in 1.08s

cargo test -p tickvault-app   (full crate — lib + every integration target + doctests)
lib target (where this suite lives):
test result: ok. 762 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.03s
All 40 `test result:` lines across the crate report 0 failed
(grep 'test result:' | grep -v '0 failed' → empty).

cargo clippy --workspace --no-deps   (CI's exact invocation, ci.yml:160)
→ finished; ZERO warnings in groww_bridge.rs (the 4 pre-existing workspace
  warnings — muhurat.rs, feed_state_persist.rs, main.rs ×2 — are unchanged,
  per the ci.yml note). Extra belt: `cargo clippy -p tickvault-app
  --all-targets --no-deps` also reports zero groww_bridge.rs lints.

banned-pattern-scanner.sh            → exit 0 (clean)
plan-verify.sh                       → PASS: All 7 plan items verified (active-plan.md VERIFIED)
per-item-guarantee-check.sh          → summary: 48 PASS, 0 FAIL, 18 SKIP (this plan PASSes)
plan-gate.sh "$PWD"                  → PASS — satisfied by active-plan-groww-retail-resume-tests.md
                                       (re-run after commit so the gate resolves via THIS plan,
                                       not the prior commit's PLAN-EXEMPT)
```

### Negative proof (recorded)

Sabotage applied to `test_retail_resume_mid_file_no_double_read` — the
zero-duplicate assertion flipped to expect a byte-0 duplicate re-read:

```rust
// SABOTAGE (temporary): expect a duplicate re-read
assert_eq!(state.live_writer.pending(), 5, "SABOTAGE: resumed drain must parse ONLY the tail");
// was: assert_eq!(state.live_writer.pending(), 2, "ONLY the 2 unflushed tail lines parsed — …");
```

Observed failure (then reverted — final run green):

```
thread 'groww_bridge::tests::test_retail_resume_mid_file_no_double_read' (12145) panicked at crates/app/src/groww_bridge.rs:3286:13:
assertion `left == right` failed: SABOTAGE: resumed drain must parse ONLY the tail
  left: 2
 right: 5
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 761 filtered out; finished in 0.60s
```

## Rollback

Test-only change confined to the `mod tests` block of one file: revert the single
`test(groww)` commit (`git revert <sha>`) — zero production-code surface, zero
schema/config/deploy impact. The plan-file commit can stay (docs) or be reverted
independently.

## Observability

This block adds tests, not runtime code — no new metrics/logs/alerts are added,
and none are removed. The mechanics under test ARE the observability-adjacent
recovery path, and the suite asserts against the real observable state the
production loop uses (`offset`, `residual`, `capture_seq`, writer `pending()`).
Existing runtime observability for this path (unchanged, now execution-proven):
`tv_groww_bridge_offset_resume_total{outcome}` counters,
`tv_groww_bridge_archive_tail_drains_total`, the resume/drain `info!`/`warn!`
lines, and FEED-SUPERVISOR-01 on bridge death. Metrics emitted during tests go
to the no-op recorder (no assertion on them — honest: counters are NOT asserted).

## Per-Item Guarantee Matrix

The full 15-row + 7-row matrices apply by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (all 15 + 7 rows confirmed
for the single item in this plan). Per-item specifics:

| Row | This item's specifics |
|---|---|
| Testing coverage | Adds the missing unit/integration coverage for the Groww recovery flow (9 tests); categories: unit + integration (temp-dir I/O), deterministic, CI-runnable |
| Zero ticks lost | Tests PROVE the no-loss/no-dup mechanics (exact line counts across partial lines, shrink, resume, archive drain); no new tick-drop path introduced (test-only) |
| Never slow/locked/hanged | No hot-path change; tests bounded (fail-fast port-1 conf, ≤350 ms flush ladder, wake-capped archive loop) |
| O(1) / performance | N/A — test-only; no production code touched |
| Uniqueness + dedup | Resume restores `capture_seq` (asserted) so the replay-stable DEDUP tiebreaker survives restart |
| Code review | Adversarial self-review pass on the diff + negative-proof sabotage recorded above |
| Extreme check | The suite itself is the new ratchet: any regression in offset/resume/archive mechanics now fails the build |
