# Implementation Plan: wire the missing QuestDbReconnected recovery alert

**Status:** APPROVED
**Date:** 2026-06-12
**Approved by:** Parthiban — selected "Wire missing observability alerts (Recommended)" via AskUserQuestion 2026-06-12, after the deep-research census established that Option B (delete dead depth variants) was already complete and the real gap is 21 defined+tested-but-never-emitted operator alert variants (audit-findings Rule 13).

**Guarantee matrices:** See `per-wave-guarantee-matrix.md` (cross-referenced). This is a single-variant observability wire: no new pub fns on a hot path, no new QuestDB table, no new dep, no OMS/strategy/indicator code (§28 respected).

---

## Design

**The gap (audit-findings Rule 11 + Rule 13 — false-OK / operator-anxiety class):**
`QuestDbDisconnected` (Severity::Critical) IS emitted from the 5-minute background
health loop at `crates/app/src/main.rs:4767` when QuestDB liveness (`SELECT 1`)
fails `>= QUESTDB_LIVENESS_FAILURE_THRESHOLD` (3) consecutive times. But
`QuestDbReconnected` has **zero production emitters** (authoritative non-comment
census 2026-06-12) — so the operator is paged "QuestDB DOWN" and is **never told
it recovered**. They cannot tell whether the incident resolved.

**The fix (symmetric, edge-triggered — audit Rule 4):** in the SAME liveness loop,
track whether a disconnect was alerted; on the next successful liveness check after
an alert, emit `QuestDbReconnected` exactly once, then clear the edge state.

**Field reshape (honesty — Rule 17 finding):** the variant currently carries
`drained_count: usize` ("total ticks/records drained from buffer"). That count is
NOT knowable at the 5-min liveness-loop site (it lives in the tick-persistence
rescue ring, a different subsystem). Passing a hardcoded `0` would violate the
sibling `QuestDbDisconnected.signal` field's explicit "Never a hardcoded zero —
always reflects reality" contract. Since the variant has zero prod callers, reshape
the field to what the emit site truthfully knows:
`failed_checks_before_recovery: u32` (the peak consecutive liveness failures observed
before recovery ≈ downtime in 5-min units). Update the `to_message()` arm + the two
test references accordingly.

**Severity unchanged:** `QuestDbReconnected` stays Severity::Medium — a recovery
confirmation, not a page (the disconnect was the page).

**Loop edits (`main.rs`, the existing `background periodic health check` task):**
- Add `let mut questdb_disconnect_alerted = false;` + `let mut questdb_peak_failed_checks: u32 = 0;` alongside the existing `last_*_alert` state vars (before the loop).
- Failure branch: `questdb_peak_failed_checks = questdb_peak_failed_checks.max(failures);` and on the `>= THRESHOLD` alert path set `questdb_disconnect_alerted = true;`.
- New recovery branch (`else if liveness_outcome.is_success() && questdb_disconnect_alerted`): emit `QuestDbReconnected { writer: "liveness-check", failed_checks_before_recovery: questdb_peak_failed_checks }`, then reset both edge vars.

**Scope (one complete vertical slice, not skeletons — Rule 14):** ONLY
`QuestDbReconnected`. The other 20 dead variants get a documented per-variant
disposition table in the PR body (wire-later / retire / leave-suppressed) so the
operator has the full map; each needs its own decision and is out of scope here.

## Edge Cases

- **Flap (down→up→down within the cooldown):** edge state is a plain bool reset on each recovery emit, so each genuine recovery edge fires exactly once; a re-disconnect re-arms it. No level-triggered spam.
- **App boots with QuestDB already healthy:** `questdb_disconnect_alerted` starts false → no spurious reconnect alert. Correct (nothing to recover from).
- **Failures rise but never cross the alert threshold (1–2), then recover:** disconnect was never alerted, so `questdb_disconnect_alerted` stays false → no reconnect alert. Symmetric — we only announce recovery for incidents we paged.
- **`failed_checks_before_recovery` value:** captured as the running max of `questdb_liveness_failures()` across the down period (the success path inside `check_questdb_liveness` resets the atomic to 0, so we must capture the peak in the loop, not read it post-recovery).
- **No market-hours gate (deliberate):** the disconnect alert is ungated (QuestDB matters whenever the app runs — persistence/audit are 24/7). The symmetric recovery alert is likewise ungated. Matches the existing site's behaviour.

## Failure Modes

- **Missed recovery emit (logic error):** covered by a unit test that drives the pure edge-decision helper through down→recovered and asserts exactly one reconnect.
- **Hidden consumer of the renamed field:** repo-wide grep for `drained_count` before commit; only events.rs def + to_message + 2 test files reference it (zero prod callers). All updated in the same commit.
- **Severity/topic match non-exhaustive:** compile error (safety net) — the arms already exist; only the field name changes.
- **Concurrent main movement:** fetch + merge origin/main before push; re-verify.
- **CI divergence:** draft PR → CI green → ready → squash auto-merge → monitored to merge.

## Test Plan

- `cargo test -p tickvault-core --features daily_universe_fetcher --lib` — events module incl. updated `QuestDbReconnected` formatting test; paste real result line.
- New unit test for the pure edge helper (`should_emit_questdb_reconnected(prev_alerted, is_success) -> bool`) OR an inline-tested decision: down (alerted) → success → emit once; success again → no re-emit; never-alerted → success → no emit.
- `cargo build --workspace`, `cargo fmt --check`, CI-exact `cargo clippy --workspace --no-deps` — zero new warnings.
- Hook gates: banned-pattern, plan-gate, pub-fn-wiring (the new emit IS the call site), pub-fn-test, test-count ratchet.

## Rollback

Single squash-merged PR; `git revert` restores the prior `drained_count` field shape and removes the recovery-edge emit verbatim. No config/schema/data/dep changes. The variant returns to zero-emitter state (its pre-PR condition).

## Observability

- Net effect: the operator now receives a Severity::Medium "QuestDB persistence RECOVERED after N failed checks — buffered ticks draining" Telegram on the recovery edge of an incident that previously only ever produced a Critical "DOWN" page. Closes the false-OK / anxiety gap (audit Rule 11).
- No new metric is added (the existing `tv_questdb_alive` gauge + `tv_questdb_liveness_latency_ms` histogram already cover the dimension); this wire adds the human-facing recovery message that the gauges don't push.
- PR body carries the full 21-variant disposition table so the remaining gaps are tracked, not lost.

## Plan Items

- [ ] Reshape `QuestDbReconnected` field `drained_count: usize` → `failed_checks_before_recovery: u32` + update `to_message()` arm; wire the symmetric edge-triggered recovery emit in the main.rs health loop; update the 2 test references; add the edge-decision unit test.
  - Files: crates/core/src/notification/events.rs, crates/app/src/main.rs, crates/core/tests/event_formatting_coverage.rs, crates/core/tests/plan_p4_p5_alert_proof.rs
  - Tests: QuestDbReconnected formatting test (updated), questdb reconnect edge-decision unit test
