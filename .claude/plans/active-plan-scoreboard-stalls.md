# Implementation Plan: Dual-Feed Scoreboard PR-B — stall episode rows (StallRestarted event kind + supervisor emit + classifier activation)

**Status:** VERIFIED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — 2026-07-10 dual-feed scoreboard directive ("run these two websockets live for a month... all tracked, captured, visualized, logged, monitored, 100% automated") + the blame-attribution directive ("ensure and CAPTURE that the issue really arose from the broker side")

> Per-item guarantee matrix: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row 100% matrix + the 7-row resilience matrix) — applies to every item in this plan. Contract source: the synthesized final scoreboard design, PR-2 scope ("stall episode rows: `WsEventKind::StallRestarted` + supervisor emit (fixed slugs, `emit_ws_audit`-style best-effort try_send) + classifier stall arms activated"). This is PR-B of the 4-PR serial sequence (PR-A merged as #1473 / 4dd64195; PR-3 = Groww lag histograms; PR-4 = presence registry follow).

## Design

The 2026-07-06 exam proved the Groww sidecar's dominant failure mode — the
NATS socket dying silently and the FEED-STALL-01 watchdog killing +
relaunching the child — left ZERO durable episode record: the scorecard's
Stalls column read the `-1` sentinel and the Groww drops column was
deliberately blinded ("?"). PR-B closes that gap by reusing the EXISTING
ws_event_audit pipeline end-to-end:

1. **`crates/common/src/ws_event_types.rs`** — new `WsEventKind::StallRestarted`
   (wire label `stall_restarted`, `all()` 6 → 7). NOT an up-kind, NOT a
   plain disconnect: the scoreboard's pairing / feed-off / reconnect logic
   ignores it as a lifecycle "up" signal by construction (allowlists).
2. **`crates/common/src/feed_blame.rs`** — the four FIXED cause slugs the
   emitter stamps into `source` (shared consts so emitter ↔ aggregation ↔
   classifier can never drift): `STALL_SOURCE_SILENT_SOCKET` /
   `STALL_SOURCE_NEVER_STREAMED` / `STALL_SOURCE_AUTH_STALE` /
   `STALL_SOURCE_ENTITLEMENT`. The PR-A classifier stall arms activate
   unchanged (substring arms verified against each slug; a lockstep test
   pins every slug→(blame, reason) pair).
3. **`crates/app/src/groww_sidecar_supervisor.rs`** — the emit. Both
   `supervise_child` kill arms (classic FEED-STALL-01 stall + the §1b
   never-streamed arm, storm and non-storm alike) call
   `emit_stall_ws_audit` (best-effort `try_send`; full/closed = the
   existing AUDIT-WS-01 error + `tv_ws_event_audit_dropped_total`,
   NEVER a supervision stall). Cause derivation: the pipe drains latch the
   child's last CONFIRMED reject class (`sets_auth_rejected` split —
   AuthRejected/EntitlementRejected only; generic `Error` never latches; a
   streaming recovery clears) into a shared relaxed `AtomicU8`
   (`stall_cause_latch_update`); `stall_cause_slug(arm, latch)` picks the
   slug (confirmed reject outranks the arm's generic slug). Row shape:
   `feed='groww'`, `ws_type='groww_bridge'`, `connection_index = conn_id`
   (§34 fleet identity), `down_secs` = the silent window, FIXED reason
   literal — never raw child text (redaction discipline). The
   `ws_audit_tx` is threaded `spawn_supervised_groww_sidecar_supervisor` /
   `spawn_groww_scale_fleet` → `run_groww_sidecar_supervisor` →
   `supervise_child`; main.rs hands both spawn sites a fresh
   `spawn_ws_event_audit_consumer` sender (same consumer pattern as the
   bridge / Dhan pool / order-update).
4. **`crates/app/src/feed_scoreboard_boot.rs`** — the aggregation maps
   `stall_restarted` rows to episode kinds
   (`episode_kind_for_event(event_kind, source)`: `stall_never_streamed`
   → `never_streamed_restart`, every other slug → `stall_restart`),
   threads the source slug into the classifier's `stall_reason`, and
   stamps `detector='stall_row'`. The existing tally fold already counts
   both stall kinds in the `stalls` column + the blame split (never the
   disconnect columns).
5. **Operator surface** — main.rs converter flips `stalls: -1 → n.stalls`
   and removes the Groww `drops_market = -1` blinding; events.rs retires
   the two PR-1 footnotes. **Stated choice (per the task's offered rule):
   stalls are real from this PR's deploy — the footnote is dropped
   entirely and 0 means measured 0 going forward**; the runbook keeps the
   pre-ship-day caveat and the CloudWatch counter holds the pre-ship past.
6. **Docs** — rule files (`dual-feed-scoreboard-error-codes.md` §1/§2/§3,
   `feed-stall-watchdog-error-codes.md` §1, `ws-event-audit-error-codes.md`
   §1) + runbook (`docs/runbooks/dual-feed-scoreboard.md` — floor caveat,
   backfill note, day-1 notes, honesty checklist) state the ship date, the
   Groww-only-by-construction asymmetry (Dhan's silent-socket detection is
   WS-GAP-06/watchdog reconnects, already counted as disconnect/reconnect
   rows), and the sub-30s-reconnect residual.

- Files: crates/common/src/ws_event_types.rs, crates/common/src/feed_blame.rs,
  crates/app/src/groww_sidecar_supervisor.rs, crates/app/src/main.rs,
  crates/app/src/feed_scoreboard_boot.rs,
  crates/core/src/notification/events.rs,
  .claude/rules/project/dual-feed-scoreboard-error-codes.md,
  .claude/rules/project/feed-stall-watchdog-error-codes.md,
  .claude/rules/project/ws-event-audit-error-codes.md,
  docs/runbooks/dual-feed-scoreboard.md

## Plan Items

- [x] `WsEventKind::StallRestarted` + label/count tests
  - Files: crates/common/src/ws_event_types.rs
  - Tests: test_ws_event_kind_as_str_labels_are_stable_and_unique,
    test_all_arrays_match_variant_counts,
    test_stall_restarted_is_neither_up_kind_nor_plain_disconnect_label
- [x] `STALL_SOURCE_*` consts + classifier slug lockstep test
  - Files: crates/common/src/feed_blame.rs
  - Tests: test_classify_stall_source_slugs_map_exactly
- [x] Supervisor emit (both arms) + cause latch + threading + ratchets
  - Files: crates/app/src/groww_sidecar_supervisor.rs, crates/app/src/main.rs
  - Tests: test_stall_cause_latch_update_matrix, test_stall_cause_slug_matrix,
    test_build_stall_ws_audit_row_shape,
    test_emit_stall_ws_audit_noop_when_tx_none_and_sends_when_some,
    test_ratchet_stall_arms_emit_stall_restarted_rows,
    test_ratchet_main_wires_ws_audit_sender_into_supervisor_spawns
- [x] Aggregation mapping + detector + stall_reason threading
  - Files: crates/app/src/feed_scoreboard_boot.rs
  - Tests: test_episode_kind_for_stall_restarted_rows,
    test_stall_restarted_literal_matches_event_kind,
    test_classify_stall_row_to_episode_blame_and_detector,
    test_episode_kind_for_event_mapping (updated)
- [x] Telegram converter flips + footnote retirement
  - Files: crates/app/src/main.rs, crates/core/src/notification/events.rs
  - Tests: test_dual_feed_scorecard_body_sentinels_and_footnotes (updated),
    test_dual_feed_scorecard_groww_drops_sentinel_footnote (updated)
- [x] Rule files + runbook + plan updates
  - Files: .claude/rules/project/dual-feed-scoreboard-error-codes.md,
    .claude/rules/project/feed-stall-watchdog-error-codes.md,
    .claude/rules/project/ws-event-audit-error-codes.md,
    docs/runbooks/dual-feed-scoreboard.md,
    .claude/plans/active-plan-scoreboard-stalls.md
  - Tests: n/a (docs; the code-side ratchets above pin the contracts)

## Edge Cases

- **Storm kills:** every kill emits (storm and non-storm) — a flapping
  socket's episodes are all counted; DEDUP `(ts, …, event_kind)` keeps
  re-aggregation idempotent (distinct ts per kill).
- **Fleet children (§34, AWS-locked-out but code-live):** `conn_id` rides
  in `connection_index`, so shard children's rows never collide with
  connection 0's (I-P1-11 discipline for WS streams).
- **Auth-stale never-streamed loop:** the latch outranks the arm — the
  episode classifies ours/token_minter_stale, not broker/never_streamed
  (the FEED-STALL §1b token-stale interplay made attributable).
- **Streaming recovery then a later silent stall:** the latch clears on
  the streaming edge, so the later stall never inherits a stale
  auth/entitlement cause.
- **Unknown future latch value / slug:** `stall_cause_slug` degrades to
  the arm's generic slug (total); an unknown slug still maps to
  `stall_restart` at the aggregation (never dropped) and the classifier's
  fail-closed floor still yields a blame (never blank).
- **Pairing / feed-off / reconnect logic:** `stall_restarted` is not an
  up-kind and not `reconnected` — process-death synthesis, the feed-off
  inference, and the reconnect count all ignore it (test-pinned).
- **Pre-ship days / backfills:** read 0 stalls (no rows) — runbook caveat;
  post-ship days backfill fully (the rows are durable).

## Failure Modes

- **Audit channel full/closed at kill time:** `try_send` drops the row
  with the AUDIT-WS-01 `error!` + `tv_ws_event_audit_dropped_total` —
  the FEED-STALL-01 log/counter/pagers for the same kill still fire; the
  supervision loop is never stalled (mirror of the bridge emitter).
- **QuestDB down at flush:** the shared consumer's AUDIT-WS-01 append/
  flush arms fire (unchanged); the stall row for that window is a
  forensic gap, never a recovery-path failure.
- **No sender attached (tests / defensive):** emit is a no-op.
- **Aggregation read failure:** unchanged PR-A behavior — SCOREBOARD-01
  stages + `-1` sentinels; stall tallies ride the same episode paths.

## Test Plan

- Block-scoped per `testing-scope.md`, escalated to ALL SIX crates because
  `crates/common` changed: `cargo test -p tickvault-common -p
  tickvault-core -p tickvault-trading -p tickvault-storage -p
  tickvault-api -p tickvault-app` (sequential), plus `cargo fmt --check`
  and CI-form `cargo clippy --workspace --no-deps -- -D warnings`.
- Unit: the new tests listed per item above (slug matrices, row shape,
  emit send/noop, kind mapping, classifier activation, footnote
  retirement) + the existing PR-A suites (fold/tally/pairing) unchanged.
- Property: the existing feed_blame proptest (totality over arbitrary
  inputs) still green with the new consts (no classifier logic change).
- Guards: error_code cross-ref/tag guards (no new ErrorCode — reuse
  AUDIT-WS-01 / FEED-STALL-01), dedup_segment_meta_guard (no DEDUP key
  change), banned-pattern scan, plan-verify, plan-gate, pub-fn guards,
  test-count ratchet (count increases).
- Ratchets: the two new source-scan wiring ratchets (supervisor arms +
  main.rs sender wiring) fail the build if either emit or the wiring is
  deleted.

## Rollback

- `git revert` of the single PR-B commit restores PR-A behavior exactly:
  the converter re-blinds stalls/Groww drops to sentinels, the supervisor
  stops emitting, and the aggregation ignores unknown event kinds (its
  unknown-kind fold arm is fail-safe by design — already-written
  `stall_restarted` rows in QuestDB are simply not classified, never a
  parse error).
- Config-level: `[scoreboard] enabled = false` still spawns no
  aggregation (PR-A rollback switch, untouched); the supervisor emit is
  try_send into a consumer that always exists — no independent toggle
  needed (a dropped row is best-effort forensic, never load-bearing).
- Data: no schema change (the SYMBOL columns accept the new labels;
  DEDUP keys untouched) — rollback needs no DDL.

## Observability

- The stall rows themselves ARE the observability delta: durable
  `ws_event_audit` rows (queryable via
  `mcp__tickvault-logs__questdb_sql`) → `feed_episode_audit` rows with
  blame (detector `stall_row`) → the daily `feed_scoreboard_daily.stalls`
  column → the 15:45 IST Telegram Stalls line (measured numbers).
- Existing signals unchanged: FEED-STALL-01 warn/error + storm
  escalation, `tv_feed_sidecar_stall_restart_total` (+ the
  never-streamed attribution counter) + both CloudWatch pagers,
  AUDIT-WS-01 on any write failure,
  `tv_ws_event_audit_dropped_total{reason}` on producer drops,
  SCOREBOARD-01 stages on aggregation degrades.
- No new metric, no new alarm, no new ErrorCode (budget + reuse
  discipline); no hot-path cost (emit is cold — at most one row per
  ≥30s-threshold kill).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Market-hours silent-socket stall kill | `stall_restarted` row, source `stall_silent_socket` → episode broker/silent_socket; card Stalls +1 |
| 2 | Never-streamed 09:20 kill loop (3 kills) | 3 rows source `stall_never_streamed` → 3 broker/never_streamed episodes; restart pager unchanged |
| 3 | Auth-stale morning (minter dead), stall kill | source `stall_auth_stale` → OURS/token_minter_stale (never blamed on Groww) |
| 4 | SILENT-FEED/permissions reject then kill | source `stall_entitlement` → broker/entitlement_reject |
| 5 | Streaming recovery, then a plain stall | latch cleared → `stall_silent_socket`, not the stale auth cause |
| 6 | Audit channel full at kill | row dropped loudly (AUDIT-WS-01 + counter); supervision + pagers unaffected |
| 7 | Backfill of a pre-PR-B day | stalls = 0 (no rows) — runbook caveat; CloudWatch counter holds the past |
| 8 | Fleet child conn 3 stalls | row `connection_index=3` — no collision with conn 0 |
