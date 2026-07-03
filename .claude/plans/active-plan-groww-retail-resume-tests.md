# Implementation Plan: Groww bridge re-tail/resume recovery test suite + two zero-loss fixes (B2)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — standing autonomy directive this session, 2026-07-03 ("go ahead dude" + ultracode standing ON)

> **Scope (UPGRADED per hostile review, 2026-07-03):** the original TEST-ONLY
> block, PLUS two surgical zero-loss production fixes inside the existing Groww
> lane in `crates/app/src/groww_bridge.rs` (crate: tickvault-app / `app`) —
> both HIGHs verified by the hostile-review pass and fixed inline before the PR:
>
> - **FIX 1 — torn-line 1-line-loss window (VERIFIED REAL):**
>   `maybe_persist_offset` persisted the RAW read cursor (`self.offset`), which
>   includes a torn trailing partial line held only in the RAM `residual`. A
>   crash after the persist resumed PAST the torn line's start; its tail parsed
>   as malformed and 1 line was permanently lost. Fix: persist
>   `self.offset.saturating_sub(self.residual.len() as u64)` — the last
>   COMPLETE-line boundary.
> - **FIX 2 — absent-live-file skipped the archive drain (H-3 incomplete):**
>   `try_resume_from_snapshot` early-returned at the `no_file` arm BEFORE the
>   archive branch — a cross-day snapshot with an existing archive was silently
>   never drained when today's live file did not exist yet at bridge start
>   (midnight rotation; sidecar first-writes ~09:00; bridge boots 08:31). Fix:
>   the archive block is extracted into a private helper
>   `drain_archive_tail_if_needed` and called from BOTH the `no_file` arm and
>   the identity-mismatch arm; all fail-closed guards unchanged.
>
> No new feeds/WS/scope — the groww-second-feed lock is untouched; both fixes
> are cold-path (resume/persist), the tick hot path is unaffected.

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
unreachable from the `tests/` dir; a child module has access, so **no
testability seam is needed**). The ONLY non-test code lines are the two
hostile-review fixes described below (one expression + one extracted helper);
no seam, no new const, no new pub fn.

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

### Production-fix design (hostile-review upgrade, 2026-07-03)

**FIX 1 (`maybe_persist_offset`)** — exact change: ONE expression,
`offset: self.offset` → `offset: self.offset.saturating_sub(self.residual.len() as u64)`.
Safety reasoning (field-by-field, as demanded):
- `capture_seq` advances ONLY on parsed COMPLETE lines, so the persisted value
  already corresponds to the complete-line boundary — no adjustment needed.
- `head` is the file's FIRST 256 bytes — tail-independent — no adjustment.
- `file_len` is diagnostic-only (`resume_from_snapshot` never reads it) — left
  as the raw metadata length, documented in the code comment.
- The F2 rotation guard (`file_len < self.offset`) deliberately keeps comparing
  the RAW cursor — the strictest shrink detection (adjusted would be weaker).
- Interactions confirmed safe: the shrink path clears `residual` + resets
  `offset` before any subsequent persist; the archive-drain loop clears
  `residual` after the drain, and any mid-drain persist (flush-Ok during the
  drain) benefits from the same boundary adjustment against the ARCHIVE file.
- `saturating_sub` is a defensive belt: residual bytes are always a suffix of
  the bytes counted in `offset`.

**FIX 2 (`try_resume_from_snapshot`)** — the archive block (previously inline in
the identity-mismatch arm only) is extracted VERBATIM into the private helper
`drain_archive_tail_if_needed(&mut self, tick_file_path, &snap, today_ist_day,
feed_health)` (pure gate unchanged: `should_drain_archive`; wake cap, no-progress
break, `active_drain_ist_day` stamp/clear, `residual.clear()` all preserved).
The `no_file` arm now calls the helper BEFORE its early return (then
`self.offset = 0` + the unchanged `no_file` log/counter). `today_ist_day` is
computed once before the metadata check (previously computed twice, ms apart —
same semantics). All fail-closed guards are byte-identical: corrupt snapshot,
wrong-day-no-archive, oversize-offset, empty-head — the pre-existing 9 tests
stay green unmodified. Observability: the recovered path logs via the EXISTING
`info!("… draining rotated archive tail …")` + the
`tv_groww_bridge_archive_tail_drains_total` counter (no new ErrorCode needed —
no error condition; tag-guard clean, verified by `error_code_tag_guard` run).

### Honest envelope (stated plainly, per operator-charter §F)

- The flush-`Ok` ARM itself is still not reachable in CI (no QuestDB), but the
  torn-line pinning test now drives `maybe_persist_offset` **DIRECTLY** (same
  private-method access), so the REAL production snapshot writer — including
  FIX 1 — is executed; only the "flush succeeded → persist called" wiring
  remains pinned by the source-scan ratchet `test_offset_persist_is_after_flush_ok`
  + the QuestDB-gated e2e. The other resume tests hand-write the snapshot JSON
  (round-trip pinned by `test_offset_snapshot_roundtrip`).
- The archive wake-CAP arm (`GROWW_ARCHIVE_DRAIN_MAX_WAKES` = 4096 wakes ≙
  16 GiB) and the no-forward-progress break (a mid-loop read fault) remain
  **unexecuted** — exercising them needs a 16 GiB fixture or mid-loop fault
  injection, and adding a test-only const/seam is forbidden. Open envelope
  note. The multi-wake LOOP itself (≥2 bounded 4 MiB wakes, chunk-seam line
  split) IS executed by `test_retail_archive_drain_spans_multiple_bounded_wakes`.

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
10. **(FIX 1)** Torn trailing line at persist time → the snapshot excludes the
    residual bytes; crash + restart + sidecar-completes-the-line → the torn
    line is recovered exactly ONCE (pre-fix: dropped as malformed = 1-line loss).
11. **(FIX 2)** Cross-day snapshot + archive present + NO today-file at bridge
    start → the archive tail drains anyway; today's file later appears and
    flows from byte 0 (pre-fix: archive tail silently orphaned).
12. Archive tail larger than one 4 MiB wake budget → the drain loop advances
    across ≥2 bounded wakes; the chunk-seam line split is carried in the
    residual — exact counts, no seam loss/duplication.

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

The two fixed production failure modes (both hostile-review HIGHs, now pinned):

- **Torn-line crash-loss (FIX 1):** persist raw cursor → crash → resume
  mid-line → torn tail parses as malformed → 1 line permanently lost with only
  a "skipping malformed tick line" log. Now: the snapshot points at the last
  complete-line boundary; resume re-reads exactly the torn bytes; test (10)
  fails on regression (negative proof recorded: `left: 0 right: 1`).
- **Orphaned archive tail on absent live file (FIX 2):** midnight rotation +
  bridge boots before the sidecar creates today's file → `no_file` early return
  skipped the archive drain → yesterday's unflushed rows silently lost when the
  2-day archive retention wiped them. Now: the archive drains from the flushed
  offset even without the live file; test (11) fails on regression (negative
  proof recorded: `left: 0 right: 3`).

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
- [x] FIX 1 (torn-line persist) + pinning test
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_retail_persisted_offset_excludes_torn_partial_line
- [x] FIX 2 (archive drain without live file) + pinning test
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_retail_cross_day_archive_drains_even_without_today_live_file
- [x] Multi-wake archive chunking test (MEDIUM follow-up)
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_retail_archive_drain_spans_multiple_bounded_wakes
- [x] Verification battery: `cargo fmt --all` + `--check`; scoped
  `cargo test -p tickvault-app groww_bridge` + full `cargo test -p tickvault-app`;
  `cargo clippy --workspace --no-deps` (CI's exact invocation — ci.yml:160);
  banned-pattern-scanner; plan-verify; per-item-guarantee-check; plan-gate;
  error_code_tag_guard (moved/new log lines).
- [x] NEGATIVE proofs: (i) mid-file resume duplicate sabotage; (ii) FIX 1
  reverted in isolation → pinning test FAILS (snapshot boundary + downstream
  1-line loss both shown); (iii) FIX 2 reverted in isolation → pinning test
  FAILS (archive orphaned). All recorded below, all restored.

### Verification outputs (pasted after execution — FINAL, post-fixes)

```
cargo fmt --all && cargo fmt --all --check   → clean (exit 0)

cargo test -p tickvault-app --lib groww_bridge   (scoped filter)
test result: ok. 79 passed; 0 failed; 0 ignored; 0 measured; 686 filtered out; finished in 2.17s

cargo test -p tickvault-app   (full crate — lib + every integration target + doctests)
lib target (where this suite lives):
test result: ok. 765 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.03s
All 40 `test result:` lines across the crate report 0 failed
(grep 'test result:' | grep -v '0 failed' → empty).

cargo test -p tickvault-common --test error_code_tag_guard
test result: ok. 2 passed; 0 failed (moved archive-drain log lines stay guard-clean)

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

### Negative proofs (recorded)

**FIX 1 reverted in isolation** — the persist expression temporarily restored to
the pre-fix raw cursor (`offset: self.offset`); the pinning test FAILS at the
snapshot-boundary assertion:

```
thread '…test_retail_persisted_offset_excludes_torn_partial_line' (27493) panicked at crates/app/src/groww_bridge.rs:3618:13:
assertion `left == right` failed: persisted offset must be the last COMPLETE-line boundary — the torn residual bytes must be EXCLUDED (the fix)
  left: 490
 right: 420
test result: FAILED. 0 passed; 1 failed; … 764 filtered out
```

…and with the two upstream asserts bypassed to expose the DOWNSTREAM loss, the
old behavior demonstrably LOSES the torn line (recovered 0 of 1):

```
thread '…test_retail_persisted_offset_excludes_torn_partial_line' (28958) panicked at crates/app/src/groww_bridge.rs:3650:13:
assertion `left == right` failed: the torn 4th line is recovered exactly ONCE — zero lost (pre-fix: 0, dropped as malformed), zero duplicated (a flushed line re-read would show >1)
  left: 0
 right: 1
```

Fix + asserts restored; final run green.

**FIX 2 reverted in isolation** — the `no_file` arm temporarily restored to the
pre-fix early return (archive drain skipped); the pinning test FAILS showing the
orphaned archive tail:

```
thread '…test_retail_cross_day_archive_drains_even_without_today_live_file' (30141) panicked at crates/app/src/groww_bridge.rs:3703:9:
assertion `left == right` failed: archive tail drained EVEN THOUGH today's live file is absent — 3 unflushed lines recovered (pre-fix: 0, silently orphaned)
  left: 0
 right: 3
test result: FAILED. 0 passed; 1 failed; … 764 filtered out; finished in 0.25s
```

Fix restored; final run green.

**Original suite sabotage** — applied to
`test_retail_resume_mid_file_no_double_read`; the zero-duplicate assertion
flipped to expect a byte-0 duplicate re-read:

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

Three independently-revertable commits on one file:
- the `test(groww)` suite commit — zero production surface;
- the FIX 1 commit (`fix(groww): persist re-tail offset excluding torn partial
  line …`) — reverting restores the pre-fix 1-line crash-loss window (its
  pinning test then fails, loudly signalling the regression);
- the FIX 2 commit (`fix(groww): drain cross-day archive even when today's live
  file is absent …`) — reverting restores the orphaned-archive behavior (its
  pinning test then fails).

Both fixes are cold-path (resume/persist at bridge start + throttled snapshot
write) — no schema/config/deploy impact; rollback is a plain `git revert <sha>`
per commit. The plan-file commit can stay (docs) or be reverted independently.

## Observability

No new metrics/logs/alerts are added, none are removed. Considered per the
coordinator's directive: neither fix warrants a NEW code — FIX 1 changes only
the persisted value (the existing resume `info!` lines report the adopted
offset), and FIX 2's recovered path already logs via the EXISTING
`info!("… draining rotated archive tail …")` +
`tv_groww_bridge_archive_tail_drains_total` (moved verbatim into the helper),
followed by the unchanged `no_file` info + counter — so the absent-live-file
drain is fully visible in logs + metrics with existing idioms. No known code
prefix appears in any message without a `code` field (`error_code_tag_guard`
run green). The suite asserts against the real observable state the production
loop uses (`offset`, `residual`, `capture_seq`, writer `pending()`).
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
| Testing coverage | Adds the missing unit/integration coverage for the Groww recovery flow (12 tests: 9 mechanics + 2 fix-pinning + 1 multi-wake); deterministic, CI-runnable |
| Zero ticks lost | Two verified loss windows CLOSED (torn-line crash-loss; orphaned archive tail) + tests PROVE the no-loss/no-dup mechanics with exact line counts |
| Never slow/locked/hanged | Both fixes are cold-path (bridge-start resume + throttled snapshot write); tick hot path untouched; tests bounded (fail-fast port-1 conf, ≤350 ms flush ladder, wake-capped archive loop) |
| O(1) / performance | Hot path untouched; FIX 1 adds one saturating_sub on the throttled (≥5 s) cold persist; FIX 2 is a code move into a helper |
| Uniqueness + dedup | Resume restores `capture_seq` (asserted); the boundary-adjusted offset re-reads only never-appended bytes, so replay stays DEDUP-idempotent |
| Code review | Hostile-review findings fixed inline; negative proofs for BOTH fixes + the suite sabotage recorded above |
| Extreme check | The suite is the ratchet: reverting either fix (proven) or regressing offset/resume/archive mechanics fails the build |
