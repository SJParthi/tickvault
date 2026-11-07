# Implementation Plan: 2026-10-20 seven-agent code audit fixes

## Preserved #2152 continuation — 2026-10-31

The user authorized resuming the preserved speed/health dashboard on green
#2158, with honest strict-target UNKNOWN/failure states. Its scoped design,
edge cases, failure modes, tests, rollback and observability are maintained in
[`speed-2152-resume.md`](speed-2152-resume.md). This child plan belongs to this
active audit plan; no unrelated active plan is archived to make room for it.
The continuation preserves original #2152 history and proceeds only to a
reviewed, native-gated draft PR; it does not merge, deploy, publish environment
configuration, or repeat withheld probe/fuzz work. Runtime/coverage/session
limitations remain explicit. The child plan cross-references the mandatory
15-row and 7-row matrices and records scope-specific validation, not guarantees.

**Status:** IN_PROGRESS
**Date:** 2026-10-20
**Approved by:** Parthiban, 2026-10-20: "Dee fic everything dude I need zero defects zero errors dude"

Source: the seven parallel read-only audits of 2026-10-20 (Rust-only sweep, hot
path, storage and audit, hostile review of the last 8 commits, zero-tick-loss and
reconnect paths, build run, branch check). One PR per lettered group, serially
(pr-completion-protocol.md).

Crates touched across the plan: `tickvault-app` (crates/app), `tickvault-core` (crates/core), `tickvault-storage`
(crates/storage), `tickvault-common` (crates/common/tests guards), `tickvault-trading`
(crates/trading, risk only; indicator/ and strategy/ need their own §28 approval).

## 2026-10-31: resumed security and order telemetry integration

**Scope status:** IN_PROGRESS — local workspace checks passed; publication and merge pending.
**Authorization:** the user's renewed instruction to resume paused work, relayed
by the coordinator on 2026-10-31, explicitly names these three previously tested
patches. This records existing session authorization; it does not invent a new
exception, authorize a live recovery, or approve a deployment.

Base: `0fd844582cbdac209f54e4394459b799f2ba797a`. Isolated independent checkout:
`/tmp/tickvault-integrate-security`, branch `security/recovery-scanner-order-gauge`.
The original checkout, original branches and saved component patches stay intact.
This scope uses the existing active plan because the repository already has five
active plans; no unrelated plan is archived and no plan ceiling is increased.

### Design

MOV1: `compute_for_list` no longer returns early when closed. Closed → window `closed_session_window(now)` and an upper bound `ts <=` in the SQL; open → unchanged 15-minute live read. The wire validators accept `closed` with priced rows.

Integrate exactly the reviewed FORCE_DISK normalization, secret-scanner output
redaction and order-update active-gauge lifetime fixes. Normalize the workflow's
remote-template value to literal yes/no while preserving every existing market
and Muhurat gate. Report scanner category/path/line while omitting matched source;
retain every detection/filter/exit policy. Make a private Rust scope guard own the
connected order socket's existing gauge through login/read and every future drop.
Use the small borrowed session struct and generic in-memory transport boundary
already validated in the component proposal; add no new lint exemption.

Guarantee matrix: cross-reference the full 15-row and 7-row matrix in
`.claude/rules/project/per-wave-guarantee-matrix.md`. For this scope, security is
the three behavioral boundaries above; performance is constant work at connection
entry/drop, not a per-frame addition; schemas, financial arithmetic, persistence,
dependency pins, runtime ownership and alarm policy are unchanged. No universal
coverage, losslessness, strict latency or future production guarantee is claimed.

### Edge Cases

MOV1: before 09:15, weekend or holiday → empty `closed`; a stock with no trade that day is simply absent (counted as unpriced); a query failure after the close reads `unreachable` / `query_failed`, never `closed`.


Only exact lowercase yes opts into FORCE_DISK; empty, malformed and shell/sed
metacharacter inputs become no. A scanner detection still blocks even if its
matching line begins with a credential, contains colons or is short. Clean input
still succeeds. The order gauge must clear after login-write error, dropping a
pending login at the test deadline, task abort, unwind and sequential sessions. It stays one during
a live connected session. Release panic=abort cannot execute destructors; process
termination and overlapping independent sockets are outside the tested contract.

### Failure Modes

MOV1: the whole-session read is slower than the 15-minute live read; it gets its own 8 s timeout (measured ~4 s round trip incl. portal on 2026-11-03, 743 NSE_EQ stocks), under the helper's 12 s capture bound.


Patch drift must be resolved without copying an older workflow over the newer
Muhurat logic. The scanner must not weaken a regex, exclusion or blocking exit to
hide output. The gauge fix must not alter login, authentication, reconnect,
watchdog or message handling. Missing external build resources leave validation
pending; earlier component tests are evidence for those snapshots, not a pass on
this integration. Source preparation runs no recovery, service, broker or AWS call.

### Test Plan

MOV1: api unit tests for the window, the SQL upper bound, closed-with-rows, closed-empty and closed-failure; portal content pin re-pinned for the message text.


- [ ] S1. Normalize the actual workflow substitution and retain a Rust behavioral guard.
  - Files: .github/workflows/emergency-fs-recover.yml, crates/common/tests/github_workflow_guard.rs
  - Tests: emergency_fs_recover_normalises_force_disk_input
- [ ] S2. Omit source snippets from scanner reports while preserving detection.
  - Files: .claude/hooks/secret-scanner.sh, crates/common/tests/secret_scanner_redaction.rs
  - Tests: secret_scanner_blocks_credentials_without_printing_source, secret_scanner_still_accepts_clean_input
- [ ] S3. Own the active gauge for the complete connected order session.
  - Files: crates/core/src/websocket/order_update_connection.rs, crates/core/src/websocket/order_update_connection/tests.rs
  - Tests: test_regression_order_update_active_gauge_login_send_error, test_regression_order_update_active_gauge_login_timeout, test_regression_order_update_active_gauge_task_abort, test_regression_order_update_active_gauge_unwind, test_regression_order_update_active_gauge_sequential_sessions

Local validation on this exact base: eight focused positives; seven intended
original-behavior regression failures, followed by restored positives; 100 common
guards, 27 lifecycle guards and 118 order-connection tests; all-target workspace
Clippy with warnings denied; full workspace 15,441 passed, zero failed, 33 existing
ignores; 11 dedicated Loom tests. That full-workspace run precedes the final
test-module packaging below; production source is byte-identical. Commands used
Rust 1.95.0, offline/locked, one build job and one test thread.
The first workspace attempt could not bind sandbox loopback sockets. The permitted
rerun exposed the production-panic guard's classification of the external fixture
filename. The fixture moved to canonical `tests.rs`. The native banned-pattern
scanner then required the real test boundary to be visible inside that file, so
the fixture now also sits in an explicit `#[cfg(test)] mod tests`. Its body is
byte-identical after removing indentation and adjusting the parent-import depth;
all assertions and production logic are unchanged. No guard or allowlist changed.
After the final wrapper, all five lifecycle tests pass, original behavior fails
all five, and restored behavior passes all five again; 63 panic/Rust-only/shell/
source-size guard tests and workspace all-targets Clippy pass. The actual banned
scanner now passes too. Earlier failures remain recorded in external artifacts.

Independent source review found no blocker, including the canonical test-layout
follow-up. Normal commit/push gates precede publication;
record their actual results and any optional-tool skips in the PR. Keep logs
bounded and never print matched secret source. S1–S3 remain open as merge-completion
items; their local implementation and tests are complete. The draft targets `main`
for required CI, explicitly depends on unmerged #2158, and gives reviewers the
focused `0fd844582..HEAD` range. Do not bypass hooks, merge, auto-merge or deploy.

### Rollback

MOV1: revert the PR; the page goes back to a blank table after the close. No data, schema or AWS change.


Each component can be reverted through ordinary reviewed Git changes if needed;
reverting any one reintroduces its documented bug, so assess that consequence.
There is no infrastructure, deployment, credential rotation or live rollback in
this task. The original saved component patches remain available outside Git.

### Observability

MOV1: no new metric; the page meta line shows the newest stored tick time and its age.


No new metric or alarm is added. `tv_order_update_ws_active` now returns to zero
when its connected future exits or is dropped. Scanner findings retain their
category, path, line and blocking status without source content. Workflow recovery
behavior is only reviewed/tested on inert templates; no real recovery is run.

## Plan Items

### PR A — depth rows the WAL refused never wait past 500 ms, plus backup-dedup allocation proof (app)

- [x] A1. Hand off unbacked depth rows from the frame arm when they have waited
  `FLUSH_INTERVAL`, so a saturated ring (biased select, timer arm not polled)
  cannot hold them in RAM for seconds. O(1) per frame.
  - Files: crates/app/src/dhan_feed_stack.rs, crates/app/src/dhan_feed_stack/unbacked_depth.rs
  - Tests: test_unbacked_clock_never_due_without_unbacked_rows,
    test_unbacked_clock_due_once_the_limit_has_passed,
    test_unbacked_clock_restarts_after_it_fires,
    test_unbacked_clock_resets_when_the_rows_leave,
    test_unbacked_clock_due_reads_the_wall_only_while_unbacked,
    proptest_unbacked_clock_fires_at_most_once_per_limit_and_never_late,
    the_late_depth_paths_use_the_late_trigger_and_the_live_drain_does_not
- [x] A2. DHAT test that installs a backup set and runs two-socket duplicate
  traffic through every dedup arm under the production recorder.
  - Files: crates/app/tests/dhat_main_feed_backup_dedup.rs
  - Tests: dhat_backup_dedup_admits_and_drops_without_allocating

### PR B — frame ring allocation under backlog (app, hot path)

- [x] B1. (merged #2110) Replace the frame ring's growable block list with a pre-allocated
  bounded queue plus a wake, so a backlog never allocates on the socket reader.
  Design reviewed by hot-path, security and hostile agents before code.
  - Files: crates/core/src/websocket/frame_ring.rs (new), crates/core/src/websocket/mod.rs,
    crates/core/Cargo.toml, crates/core/src/websocket/pool_supervisor.rs,
    crates/app/src/dhan_feed_stack.rs
  - Design (agent-reviewed 2026-10-20): `crossbeam_channel::bounded` storage
    (already a pinned workspace dep; all slots allocated at boot) plus a
    `tokio::sync::Notify` wake; strong-sender count for the close, so the
    drain still ends when every socket is gone; `recv` wrapped in
    `tokio::task::coop::cooperative` so a backlog still yields like tokio's
    channel; register (`Notified::enable`) before checking the queue, so no
    wake is lost.
  - Tests: test_frame_ring_delivers_in_order_then_none_after_the_last_sender,
    test_frame_ring_try_send_refuses_past_capacity_and_hands_the_item_back,
    test_frame_ring_try_send_after_the_receiver_is_dropped_is_closed,
    test_frame_ring_zero_capacity_is_raised_to_one,
    test_frame_ring_downgrade_gives_a_weak_sender_that_does_not_keep_the_ring_open,
    test_frame_ring_upgrade_succeeds_while_a_strong_sender_lives,
    test_frame_ring_try_recv_len_and_is_empty_track_the_queue,
    test_frame_ring_a_cloned_sender_keeps_the_ring_open,
    test_frame_ring_cross_thread_wake_never_loses_an_item,
    test_frame_ring_recv_dropped_inside_select_loses_nothing,
    test_frame_ring_a_full_backlog_yields_to_other_tasks,
    dhat_wal_ring_sink_accept_allocates_nothing_in_steady_state_backlog_or_shed

### PR C0 — WAL apply-lag latch releases on a staircase drain (storage) — live disk risk

Found by the 2026-10-20 live AWS check (read-only): on 14, 15 and 16 Oct the
after-close depth pass wrote 0 rows and cleared 0 buckets (stop: Deadline)
while pinned WAL depth segments grew 74.6 → 118.1 GB against an 80.5 GB
ceiling. The pass pauses on the apply-lag latch; `market_depth` drained its
backlog 12 txn at a time with flat polls between, and the latch reset its
fall count on every flat poll, so it never released from late morning to
17:15. Operator chose "Pass ignores steady lag (Recommended)" on 2026-10-20.

- [x] C0. A flat poll neither counts as a fall nor resets the fall count; only a
  rise does. Jitter still never releases; a table stuck flat still latches.
  - Files: crates/storage/src/wal_suspension_watcher.rs
  - Tests: a_staircase_drain_with_flat_polls_releases_the_latch,
    a_re_arm_after_a_release_needs_two_fresh_falls,
    falls_before_the_latch_fires_do_not_count_toward_its_release,
    two_falls_an_hour_apart_do_not_release_the_latch,
    a_slow_staircase_stays_latched_instead_of_sawtoothing,
    a_rise_between_two_falls_keeps_the_latch_even_with_flat_polls,
    one_falling_poll_does_not_release_the_latch_but_two_do,
    a_lag_pinned_perfectly_flat_above_the_floor_still_latches,
    an_uneven_staircase_re_arms_but_pages_at_most_once_an_hour,
    a_new_episode_after_the_floor_pages_at_once,
    test_untrusted_count_holds_a_staircase_release_for_the_watermark
  - Proof: the two staircase tests FAIL on the old rule; the four hostile-review
    tests FAIL on the first version of this change (a fall count left stale
    after a fire or release, and no expiry for an old fall); all pass now.
    A second review found re-arm pages every 8-11 polls on uneven steps and the
    watermark trusting a staircase release: pages now wait an hour inside one
    episode, and the watermark keeps the old two-falls-in-a-row rule.

### PR W — bounded WAL unapplied ranges (stops the shed-overflow WAL growth on sessions that end with clean probes) (storage, app)

Found by the 2026-10-20 live check: the catch-up backlog guard marked a
3,176-bucket span into the 128-slot unapplied table, which overflowed; the
overflow was persisted as one bit with no height and reloaded at `u64::MAX`,
so every later segment read as unapplied and the prune could delete nothing.
Converged design (three judges).

- [x] W1. Bounded unapplied ranges: at most 32 disjoint ranges (merge the two
  closest when full, coverage only grows) in a CRC-checked sidecar beside
  `applied.tvaw`; `applied.tvaw` stays version 1, keeps its overflow bit set
  while any range exists and names the sidecar generation in its pad bytes; an
  overflow carries a height in RAM. Boot fold once per process before the WAL
  threads spawn: inherited overflow to `[oldest waiting, own_floor]`, earlier
  buckets to exact ranges, boot tail `[boot_applied_floor + 1, own_floor]`.
  The backlog guard writes one range; ranges trim below the oldest waiting
  segment after the boot refold and each catch-up round; the drained reset
  clears them. The catch-up summary names an early stop (`frame_cap`,
  `memory`, `disk`, `deferred`, `ack_timeout`, `replay_failed`), never
  `drained`. NOT changed: the prune gate, deferred-depth pins, the per-frame
  shed and drain paths, catch-up caps and pacing, alarms, EMF names and
  filters. **This stops new growth; it does not shrink the backlog already on
  disk**, which stays protected until a catch-up drain reads it.
  **⚠ CHANGED 2026-10-20 (review):** the overflow bit is no longer kept set
  while any range exists (an older binary seeds it at `u64::MAX` for ever, so
  a rollback would have stopped every prune on a clean box): each range is
  also written into the version-1 bucket table (less the boot's frameless
  gap) when it fits, and the bit is set only for an in-RAM overflow, a
  migrated overflow, or a range that does not fit. The migrated range and the
  guard start a reorder slack below the oldest waiting segment; a folder that
  cannot be listed fails closed (fold from 1, guard from the boot floor, trim
  refuses); the trim clips ranges to the waiting segments' spans instead of
  only cutting below the oldest; the suspension probe and the settle recovery
  write ranges, not buckets, and the fold uses the overflow's low end when
  known.
  **⚠ CHANGED 2026-10-20 (review round 3):** the fold wrapper rounds
  `own_floor` up to a frame base and ratchets the frame counter to it, so
  no frame of this process lands in the span treated as frameless, and the
  drained reset uses the safer boundary below `own_floor`; the boot floor
  is `min(hwm_ticks, hwm_depth)` less the slack whatever the depth flag
  says; a full range set merges across the lowest gap with nothing waiting
  (else the lowest pair), the fold gets the waiting spans, and the guard
  is clamped to the boot frame high; the migrated overflow carries its span
  (sidecar version 2, version 1 still read) and clears once no range
  meets it; the trim cuts below the lowest readable span when the only
  unreadable segments sit in `settling/unreadable/`, and counts every
  refusal (local metric only); the boot tail starts no lower than the
  oldest waiting segment; the overflow low end is removed; the probe's
  range writes and an immediate persist run off the runtime worker; the
  tail probe caps a record read at 4 MiB. A backlog that never drains still
  keeps one segment per boot until it drains (stated, pinned by a
  property test over 40 to 55 boots); a full disk and a crash between the
  two file writes still fail toward replaying more (stated, not fixed).
  **⚠ CHANGED 2026-10-20 (review round 3, second pass):** the range written
  when a suspension episode ends, and the in-session overflow bound, carry
  one reorder slack on their tops, so they reach the first record of any
  segment a reordered frame of theirs can sit in (a lower margin in the
  segment check was tried and rejected: the lifetime property tests showed
  it pins the segment above every range end); the replay skip and the prune
  share one rule, `AppliedSnapshot::segment_is_applied`; the in-session
  overflow bound resets the overflow bounds after each bound (compare-and-
  swap, flag cleared first, at most four passes), so a later overflow of the
  session is bounded on its own instead of from the first one's low end; the
  overflow arms lower the low end before raising the height; the tail probe
  returns an I/O error as an error, so a segment it cannot read is never
  taken for empty. **Scope of the growth claim:** the WAL stops growing only
  on sessions that end with clean QuestDB probes. While apply lag keeps a
  table latched as untrusted, every ack parks, the episode ranges and the
  next boot's tail cover the whole unconfirmed stretch, the catch-up drain
  stops on growing lag and the settle step cannot archive, so such a
  session still adds its WAL to the pinned set (Assumed common on the live
  box: apply lag measured 11,503 to 27,089 on 2026-09-08). **⚠ CORRECTED
  2026-10-21 (round 6):** "adds its WAL to the pinned set" understates it.
  ONE suspicion episode between the catch-up drain's first acked batch and
  its promotion to `archive/` (the drain's own apply-lag pause starts one:
  reported tables are a subset of untrusted ones; a failed probe does too)
  holds the whole process's settled output for the next boot to read again,
  so that boot's drain progress is undone and the migrated backlog does not
  shrink at all; pinned bytes stay flat. See (E) in the round-6 section and
  plan item W3. Verify after
  deploy over several sessions: the suspicion episodes per session
  (`AppliedWatermark::suspect_episode_count`, not published as a metric;
  `tv_questdb_wal_apply_lag_max` is the published proxy; **⚠ CORRECTED
  2026-10-20 (round 5): superseded by round 4 (C) below, which publishes
  `tv_wal_suspect_episodes_total` and `tv_wal_suspect_seconds_total`**) and
  `tv_wal_unapplied_pinned_bytes` / `tv_wal_unapplied_pinned_segments`;
  pinned bytes rising every day means the live box still grows. The
  structural fix (confirm parked live acks once a full durability lag and
  two clean probes follow them, as `settling/` already does for replays) is
  NOT in this PR and needs its own plan item (see "Follow-up" below).
  **⚠ CHANGED 2026-10-20 (PR W review round 4).** (A) Per-frame shed
  BUCKET marks (`note_unapplied`, bucket = `seq >> 38`) carry no reorder
  slack, so a frame shed just below a `2^38` boundary inside a segment whose
  first record is just above it made that segment read applied (skipped by
  replay, prunable). `segment_is_applied` now reads the bucket table and the
  in-RAM overflow height from `reorder_reach_lo(first)` (one slack lower,
  never below 1); the range lookup stays at `[first, hi]`, so checks G and
  M3 are unchanged except that G no longer expects a segment starting
  within one slack above the boot's shared bucket to read applied (the
  stated cost). The boot fold gives each folded bucket one slack on its
  top; the deferred-depth prune lookup and `segments_for_bucket` (the
  after-close pass) reach one slack below a segment's first record. The
  lifetime property test starts one new segment in ten just above a bucket
  boundary with a record half a slack below its first; seed
  `16702864118144502203` (pinned as a plain test) fails Z on the old rule.
  Rollback limit: an older binary still reads bucket marks from the first
  record (check O exempts exactly that shape; **⚠ CORRECTED round 5:** the
  filter exempted every frame outside the segment's buckets, whatever
  protected it; now only a shed frame, on either side). (B) Honest limit: WAL growth
  can also continue through the deferred-depth table, whose overflow is
  still sticky and height-blind (W2 below). (C) The suspect state is now
  published, local `/metrics` only, registered at 0 at boot:
  `tv_wal_suspect_episodes_total` and `tv_wal_suspect_seconds_total` (whole
  seconds, remainder carried; O(1) once per watcher poll, never per frame).
  **Post-deploy verify list (replaces the one above):**
  `tv_wal_unapplied_pinned_bytes` and `tv_wal_unapplied_pinned_segments`,
  read together with `tv_wal_suspect_seconds_total` and
  `tv_wal_suspect_episodes_total` per session;
  `tv_wal_deferred_depth_kept_segments`; and the deferred table's overflow
  flag (`overflowed` on the after-close pass's start log line, or the
  overflow flag of `deferred_depth.tvdd`). The claim "growth stopped" is
  made ONLY after pinned bytes stay flat across several sessions measured
  together with the suspect time of each; pinned bytes that rise on
  sessions with suspect time and stay flat on sessions without it mean the
  follow-up below is what remains.
  **⚠ CHANGED 2026-10-20 (PR W review round 5).** (A) The upper side of a
  segment: a segment's last record can sit up to one slack ABOVE the next
  segment's first record, so a bucket mark on it sat above the segment's
  range and the segment read applied (replay skip, prune; for shed depth the
  after-close pass never re-read it). `segment_is_applied` reads the bucket
  table up to `reorder_reach_hi(hi)`; the boot-folded buckets and the
  in-session overflow bound get one slack on their bottom too; the
  deferred-depth prune uses `segment_is_deferred` and `segments_for_bucket`
  reaches one slack above a range end. Cost: a segment ending within one
  second below a marked bucket's start is held. The property generator
  rotates one segment in ten across a boundary the other way when `mirror`
  is on; four seeds that fail Z under the old rules are pinned. (B) The
  never-draining bound, corrected: each session keeps its tail, PLUS every
  segment overlapping one of its per-frame unapplied bucket marks (ring
  shed, append failure, rescue failure), PLUS, for a session with ANY
  suspicion episode (closed or open at the stop), the episode's stretch;
  "everything else prunes" holds only for a session with no shed bucket and
  no suspicion episode. The tail starts at the depth watermark, which stops
  at the 15:40 IST depth window, so it covers every segment from the last
  in-window depth ack to the stop (count per day to be read after deploy).
  The stuck property test now sheds and closes episodes and checks that
  bound. (C) `tv_wal_unapplied_trim_refused_total{reason}` is registered
  at 0 at boot; the pinned gauges deliberately are not (0 would read
  "nothing pinned" before any trim ran).
  **Post-deploy verify list (round 5, adds to the round-4 list):** beside
  pinned bytes and suspect seconds, read the ring-shed counters
  (`tv_dhan_ws_ring_full_total{endpoint}`, of which
  `tv_dhan_ws_ring_bytes_full_total` is the byte-budget subset) and the
  append-failure counters (`tv_ticks_dropped_total{feed}`,
  `tv_depth_rows_dropped_total`) per session, so a rise in pinned bytes on a day with no suspect
  time can be put down to shed buckets; an ABSENT
  `tv_wal_unapplied_pinned_*` gauge means the trim did not run or was
  refused, read `tv_wal_unapplied_trim_refused_total{reason}` with it.
  **Limit (round 5), recorded:** during round 4, `git checkout --` on
  `crates/storage/tests/wal_unapplied_ranges_properties.proptest-regressions`
  discarded two unstaged seed lines that cannot be recovered. They are
  PROBABLY the two seeds `test_regression_round4_bucket_boundary_reorder_seeds`
  pins (Assumed; not confirmed). To regain coverage a one-off sweep of 1,024
  lifetime cases (start state, stuck and mirror drawn per seed) and 1,024
  stuck-backlog cases (40 to 55 boots) was run on the round-5 code before
  merge: 0 failures in 2,048 cases. The four seeds still in the
  regressions file are now also pinned as plain tests
  (`test_regression_saved_proptest_seeds_replay_as_plain_cases`). Pinned
  `run_case` tests, not the regressions file, are the durable form.
  **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2).** (A) The pinned
  gauges are published after the boot fold and after the backlog guard
  too, whether or not a replayed batch is acknowledged; "nothing waiting"
  publishes 0. Before, only a trim set them and a trim runs only after an
  acknowledged batch, so a box under the replay disk floor never set them
  and read the same as a clean box. **Honest limit (disk floor):** the
  migrated backlog drains only while the WAL volume stays above the replay
  disk floor (`wal_replay_disk_floor_bytes`, 40 GiB on the 600 GB box,
  unchanged), and the drain itself uses QuestDB disk (25–75 GB per
  session's backlog, measured 2026-09-03). Below the floor every pass is
  refused, nothing is trimmed, and the backlog plus every later session's
  leftovers stay pinned until space is freed. (B) Both suspension ranges
  start two reorder slacks below their watermark: a frame acked after the
  last clean probe can be the last record of a segment whose successor's
  first record sits up to one more slack below it, and that segment's
  judged range ended under a one-slack range (replay skip, prunable).
  Ranges derived from an ack watermark need two slacks on the bottom.
  (C) The trim's spans are built in write order (segment names), each
  starting at the highest first written so far less the slack; in
  first-sequence order a long segment written after a tiny one, with its
  first record just below the tiny one's, lost its middle from every span
  and a frame shed there was skipped uncounted. (D) The orphaned comment
  block in `ws_frame_spill.rs`'s tests is deleted (16 lines under the pin).
  The lifetime generator gains `extra` (rotation across `H − slack` at a
  suspicion, inverted segment pairs); seeds that fail Z under each old rule
  are pinned (`test_regression_round4_pass2_extra_generator_seeds`).
  **Post-deploy verify list (pass 2, corrects the round-5 line above):**
  read `tv_wal_unapplied_pinned_bytes` and `tv_wal_unapplied_pinned_segments`
  after each boot (they are now set every boot); allow a tolerance of one
  segment per session when judging "flat across sessions" (the gauge
  measures with the trim's spans, the prune with `[first, next first − 1]`,
  so the boundary segment can be counted on one side and not the other).
  Read with them `tv_wal_replay_disk_floor_stops_total` and the catch-up
  summary line's `stop_reason` (`disk`, `frame_cap`, `ack_timeout`,
  `apply_lag`): a rising pinned set with disk-floor stops is the deadlock
  above, not new growth. An ABSENT gauge now means only that no scan could
  read the waiting set (a folder that cannot be listed, or an unboundable
  unreadable first record outside `settling/unreadable/`); read
  `tv_wal_unapplied_trim_refused_total{reason}` for which. After a drained
  reset the gauges keep the value published before it until the next boot
  (reads high, never low).
  **⚠ CHANGED 2026-10-21 (PR W review round 6).** (A) The whole-segment
  rule (`AppliedSnapshot::segment_is_applied`) needs the segment's end
  plus one slack strictly below `skip_below` for both sinks, and the boot
  tail and the backlog guard start at `AppliedWatermark::boot_range_floor`
  (`boot_applied_floor` less one more slack, at a frame base): an unacked
  frame just above `skip_below`, the last record of a segment whose
  successor starts up to a slack below it, made that segment read applied
  with no suspicion at all (replay skip, prunable). The lifetime test gains
  `straddle` (the plain-watermark straddle, then an in-session prune and a
  crash or a clean stop); three seeds fail Z with either old rule and are
  pinned; G and M3 allow two slacks. (B) `ack_gate` (a `Mutex<()>`) around
  the suspect-flag check and the update in both ack functions and around
  the flip and reads at both probe transitions. (C) The write-order check
  compares each first with the running maximum
  (`firsts_within_slack_of_running_max`), in `order_by_name` and
  `bound_unreadable`. (D) The boot fold's summary and a failed fold
  persist's warning are written by `log_boot_fold_summary`, called from
  `main.rs` right after the subscriber's `init`. (E) Honest limit: the
  migrated backlog (and every per-session leftover behind it) shrinks only
  on a boot whose catch-up batches are promoted to `archive/`; one episode
  before the promotion holds the process's whole settled output, so a boot
  that pauses even once makes zero net progress. (F) `waiting_spans` is
  documented as the test seam it is. (G) The pinned gauges' staleness
  limit is corrected: within a session they can read low or high, and a
  present gauge can be stale.
  **Post-deploy verify list (round 6, adds to the lists above):** per boot,
  read `tv_wal_replay_confirmed_segments_total` (must be above 0 for the
  backlog to shrink), `tv_wal_replay_settle_held_segments_total` and
  `tv_wal_replay_settle_recovered_segments_total`, with the catch-up
  summary's `stop_reason=apply_lag` and `tv_wal_catchup_lag_pause_total`.
  FLAT pinned bytes with held above 0 and confirmed at 0 means the backlog
  is NOT draining, not that growth stopped. Read the pinned gauges right
  after a boot, with `tv_wal_unapplied_trim_refused_total`. The boot log
  now carries the fold's summary line ("WAL unapplied record folded at
  boot") and, on a failing disk, `source=unapplied_ranges_persist_failed_at_boot`.
  - Files: crates/storage/src/wal_unapplied_ranges.rs,
    crates/storage/src/wal_applied_watermark.rs,
    crates/storage/src/ws_frame_spill.rs, crates/storage/src/wal_replay_settle.rs,
    crates/storage/src/lib.rs, crates/storage/tests/error_level_meta_guard.rs,
    crates/storage/tests/wal_unapplied_ranges_properties.rs,
    crates/app/src/dhan_feed_stack.rs, crates/app/src/dhan_feed_stack/catchup_stop.rs,
    crates/storage/src/wal_deferred_depth.rs (doc only in round 4;
    `segment_is_deferred` in round 5),
    crates/storage/src/ws_frame_spill/tests/frame_base_tests.rs,
    crates/app/src/main.rs (counter registration, rounds 4 and 5; the
    boot fold log call, round 6),
    CLAUDE.md
  - Tests: overflow_taints_only_ranges_starting_at_or_below_its_height,
    test_regression_reloaded_overflow_clears_after_a_drain_past_its_height,
    test_v1_overflow_bit_migrates_to_range_bounded_by_own_floor,
    test_migrated_range_covers_replaying_segments,
    test_rollback_older_binary_decode_keeps_overflow_bit_while_migrated_ranges_exist,
    test_rollback_old_decoder_on_a_clean_box_sees_buckets_not_overflow,
    test_range_buckets_without_a_trusted_sidecar_fold_back_into_their_spans,
    test_migrated_range_keeps_a_record_below_its_segment_s_first_sequence,
    test_fold_migrates_a_known_overflow_up_to_its_height,
    test_waiting_segments_fail_closed_when_a_folder_cannot_be_listed,
    test_waiting_spans_clip_a_gap_to_the_segment_s_last_record,
    test_trim_to_waiting_clips_a_range_over_a_stretch_with_no_waiting_segment,
    test_unapplied_ranges_clip_to_splits_keeps_and_drops,
    test_clip_unapplied_ranges_to_clears_migrated_when_empty,
    test_suspicion_across_an_overnight_gap_is_a_range_not_an_overflow,
    test_regression_da13c_boot_rereads_segments_left_settling_by_a_crash,
    test_sidecar_to_bytes_from_bytes_round_trip,
    test_sidecar_bad_crc_magic_short_or_malformed_is_refused,
    test_corrupt_or_missing_sidecar_falls_back_to_buckets_or_an_unbounded_overflow,
    test_regression_next_day_shed_no_longer_overflows_after_boot_fold,
    test_unapplied_ranges_full_merges_lowest_pair_and_never_shrinks_coverage,
    test_trim_below_drops_and_cuts,
    test_boot_applied_floor_frozen_at_bind_and_ignores_later_acks,
    test_boot_tail_registered_before_writer_spawn,
    test_regression_multi_day_backlog_never_overflows_the_bucket_table,
    test_guard_never_calls_note_unapplied_range,
    test_regression_leftover_backlog_is_replayed_after_live_acks_pass_it,
    test_guard_pending_backlog_ignores_segments_at_or_above_the_ceiling,
    test_per_record_skip_honours_ranges_after_live_acks_pass_them,
    test_prune_never_deletes_a_segment_under_a_range_or_an_unbounded_overflow,
    test_regression_20_oct_shape,
    test_catchup_stop_reason_frame_cap_is_not_drained,
    proptest_wal_lifetimes_never_skip_or_prune_an_unapplied_frame,
    test_add_range_avoiding_merges_across_a_gap_no_span_intersects,
    test_fold_at_boot_with_spans_merges_across_a_free_gap,
    test_fold_wrapper_ratchets_the_frame_counter_past_own_floor,
    test_ratchet_frame_seq_to_makes_every_later_mint_greater,
    test_round_up_to_frame_base_rounds_up_and_saturates,
    test_drained_reset_keeps_a_suspension_range_above_the_ceiling,
    test_regression_depth_untracked_file_keeps_depth_frames_between_the_watermarks,
    test_guard_pending_backlog_is_clamped_to_the_boot_frame_high,
    test_boot_tail_starts_at_the_oldest_waiting_segment_without_a_file,
    test_migrated_flag_clears_once_no_range_intersects_the_migrated_span,
    test_sidecar_from_bytes_reads_a_version_1_file,
    test_trim_to_waiting_cuts_below_a_segment_set_apart_as_unreadable,
    test_note_questdb_probe_writes_ranges_off_the_runtime_worker,
    test_record_at_capped_refuses_a_length_past_the_cap,
    proptest_stuck_backlog_keeps_later_sessions_prunable,
    test_regression_episode_end_range_keeps_a_segment_holding_a_reordered_frame,
    test_segment_is_applied_is_the_range_check_on_both_sinks,
    test_bound_known_overflow_locked_bounds_a_known_height_only,
    test_regression_a_second_overflow_after_a_persist_is_bounded_on_its_own,
    proptest_repeated_in_session_overflows_are_bounded_one_by_one,
    test_regression_a_segment_that_cannot_be_read_is_never_taken_for_empty,
    test_overflow_arms_lower_the_low_end_before_raising_the_height,
    test_regression_bucket_mark_holds_a_segment_starting_just_above_the_boundary,
    test_regression_overflow_height_holds_a_segment_starting_just_above_it,
    test_regression_prune_keeps_a_segment_holding_a_frame_shed_below_the_boundary,
    test_regression_deferred_mark_reaches_a_segment_starting_above_the_boundary,
    test_regression_round4_bucket_boundary_reorder_seeds,
    test_reorder_reach_lo_is_one_slack_lower_and_keeps_zero,
    test_note_questdb_probe_counts_suspect_episodes_and_seconds,
    test_regression_bucket_mark_holds_a_segment_ending_just_below_the_boundary,
    test_regression_overflow_bound_holds_a_segment_ending_just_below_its_low,
    test_reorder_reach_hi_is_one_slack_higher_and_saturates,
    test_segment_is_deferred_reaches_one_slack_each_side,
    test_regression_prune_keeps_a_segment_whose_last_record_is_shed_above_the_boundary,
    test_regression_deferred_mark_reaches_a_segment_ending_below_the_boundary,
    test_segments_for_bucket_places_an_unreadable_segment_with_the_reach,
    test_regression_round5_mirror_boundary_rotation_seeds,
    test_regression_saved_proptest_seeds_replay_as_plain_cases,
    test_fold_at_boot_and_guard_pending_backlog_publish_pinned_without_an_acked_batch,
    test_regression_trim_keeps_a_range_over_a_long_segment_written_after_a_tiny_one,
    test_order_by_name_sorts_by_write_order_and_refuses_an_unknown_order,
    test_regression_suspension_range_holds_a_segment_whose_last_record_was_acked_late,
    test_suspension_range_lo_is_two_slacks_lower_at_a_frame_base,
    test_regression_round4_pass2_extra_generator_seeds,
    test_regression_plain_watermark_straddle_keeps_a_segment_whose_last_record_was_never_acked,
    test_regression_round6_plain_watermark_straddle_seeds,
    test_ack_gate_serialises_the_flag_check_with_the_update,
    test_acks_racing_probe_transitions_are_always_in_an_episode_range,
    test_order_by_name_refuses_falls_that_add_up_past_the_slack,
    test_regression_waiting_spans_cover_every_first_after_falls_that_add_up,
    test_log_boot_fold_summary_writes_the_recorded_fold_once_after_logging_starts,
    test_module_header_names_the_drain_progress_counters

- [x] W2. (done: `wal_deferred_depth.rs` `kinds_in_range` + `deferred_depth_pass.rs`, branch claude/assurance-fix-w2) The deferred-depth table honours its overflow height (found by the
  PR W review round 4; NOT in PR W). `wal_deferred_depth` has the
  sticky-overflow shape PR W removed from the unapplied table:
  `DeferredDepthSnapshot::kinds_in_range` returns `SHED_DEPTH_ALL` for EVERY
  range while `overflowed`, ignoring `overflow_high_seq`, so one collision
  pins every later segment until an after-close pass completes a sweep at
  or below the height. A trading-hours shed collides with a mark about 13
  days old (2 × 2048 × 2^38 ns = 13 d 0 h 45 min; one step of 6 d 12 h
  22 min moves the time of day outside the session). Fix: a range whose low
  end is above `overflow_high_seq` is not tainted by the overflow (an
  unknown height, `0` or `u64::MAX`, still taints everything); a known
  height is persisted in the file header that already carries it, and the
  after-close pass sweeps only up to it. Keeps more, never less: every
  range at or below the height still reads `SHED_DEPTH_ALL`.
  - Files: crates/storage/src/wal_deferred_depth.rs,
    crates/app/src/dhan_feed_stack/deferred_depth_pass.rs (if the sweep
    bound moves), CLAUDE.md (the `DeferredDepth` row)
  - Tests (to write): test_overflow_taints_only_ranges_at_or_below_its_height,
    test_unknown_overflow_height_still_taints_every_range,
    test_reloaded_overflow_height_round_trips_through_the_file,
    test_prune_deletes_an_applied_uploaded_segment_above_the_overflow_height,
    a property test that a collision never lets a prune drop a marked frame

- [x] W3. (done: `wal_replay_settle.rs` `SettleBatch`, branch claude/assurance-fix-w3) The replay settle step promotes batch by batch (found by the PR
  W review round 6; NOT in PR W). `SettleState` keeps ONE mark for every
  settled segment of the process, and `promote` holds the whole set (and,
  once `held`, every later batch) when any suspicion episode began after
  the mark. The catch-up drain's own apply-lag pause starts such an episode
  (reported tables are a subset of untrusted ones), so a boot whose drain
  pauses once makes zero net progress on the migrated backlog: the next
  boot recovers those segments, reads them first, causes lag, and holds
  again. Fix: a mark per settled batch (its own ack instant, clean-probe
  count and episode count), promote each batch whose own durability lag
  and two clean probes passed before an episode began, and hold only the
  batches whose ack came before an episode's start; `held` no longer
  sticks for the rest of the process. Keeps more, never less: a batch acked
  inside or before an episode is still held for the next boot.
  - Files: crates/storage/src/wal_replay_settle.rs, crates/storage/src/ws_frame_spill.rs
    (callers, if the API changes), CLAUDE.md (the settle row)
  - Tests (to write): test_a_batch_promoted_before_an_episode_stays_archived,
    test_an_episode_holds_only_batches_acked_before_its_start,
    test_held_no_longer_sticks_for_the_rest_of_the_process,
    a property test over interleaved batches, probes and episodes that no
    batch acked before an episode's start reaches `archive/`
### PR H — no data-silence redial on a weekend or NSE holiday (core, app) — live finding

Found by the 2026-10-20 live AWS check (read-only): on the Dussehra holiday the
frame-silence watchdog redialled every socket 569 times between 09:15 and 15:30
IST, because its gate checked the time of day only.

- [x] H1. The silence gate reads the trading calendar (`tickvault-core`
  `silence_calendar`); a weekend or NSE holiday keeps the data-silence redials
  and the 805 probe window shut, unless at least 3 sockets delivered data in
  the last 10 s (hostile review HIGH: a wrong calendar on an open market).
  - Files: crates/core/src/websocket/silence_calendar.rs, crates/core/src/websocket/mod.rs,
    crates/core/src/websocket/pool_supervisor.rs, crates/app/src/dhan_feed_stack.rs
  - Tests: test_holiday_suppresses_the_silence_redial_but_not_the_ping_watchdog,
    test_holiday_leaves_close_code_handling_unchanged,
    test_market_is_streaming_needs_three_fresh_sockets,
    test_override_on_evidence_opens_a_holiday_only_when_streaming,
    test_session_secs_of_day_now_overrules_a_holiday_while_streaming

### PR I — depth-200 10-minute minimum hold (app) — live finding

Found by the 2026-10-20 live log read (read-only, 14, 15, 16 and 19 Oct):
depth-200 swapped about 1,200 times a day, about 2 frames per swap. Owner
tapped "Hold 10 min (Recommended)" on 2026-10-20; rule recorded in
`websocket-connection-scope-lock.md` "2026-10-20 — DEPTH-200 HOLDS A SWAPPED-IN
CONTRACT FOR AT LEAST 10 MINUTES".

- [x] I1. A contract the steering loop swaps onto a depth-200 socket is not
  swapped off for 600 s (`DEPTH200_MIN_HOLD_SECS`, a constant: the rule REJECTs
  any other value). `Depth200HoldBook` keeps each socket's swap-in instant
  (monotonic, stamped after `apply_decision` returns; compared against a
  reading taken after the probe block) and the one before it, so a refused,
  reverted swap gets its original time back. `locked_mask` gives the planner
  the locked socket's underlying, so a same-underlying candidate is never put
  on a second socket (the 2026-09-24 "top 5 distinct underlyings" rule).
  Candidates the hold keeps waiting are `held_back` (logged), never
  `unplaced` / `socket_empty`. An empty socket, a dial-placed contract and a
  contract gone from the eligible set (not a stock option in today's
  contract-to-underlying map) are never held. Caps, `ROTATION_HALTED`, 805
  handling, the silent-swap resubscribe, depth-20 and the main feed unchanged;
  no new alarm, page, filter or EMF name (`min_hold` is a local `/metrics`
  label value on the existing counter).
  - Files: crates/app/src/depth200_min_hold.rs, crates/app/src/depth200_ranked_steer.rs,
    crates/app/src/depth_rebalance.rs, crates/app/src/lib.rs, CLAUDE.md
  - Tests: test_depth200_min_hold_keeps_a_contract_for_600s,
    test_depth200_min_hold_releases_after_600s,
    test_depth200_min_hold_never_blocks_an_empty_socket,
    test_depth200_min_hold_drops_an_ineligible_contract,
    test_depth200_held_underlying_needs_a_stock_option_in_todays_map,
    test_depth200_hold_book_observe_restores_the_prior_swap_in_on_a_revert,
    test_depth200_hold_book_revert_restores_the_original_swap_in_inside_the_hold,
    test_depth200_hold_book_note_sent_swaps_skips_a_swap_that_was_not_queued,
    test_depth200_hold_book_locked_mask_is_false_for_a_mismatched_or_short_book,
    proptest_depth200_min_hold_no_socket_changes_twice_within_600s,
    plan_ranked_minute_skips_a_min_hold_socket_and_funds_the_next,
    a_held_contract_keeps_its_underlying_so_a_same_name_candidate_is_not_placed_elsewhere,
    the_planner_peeks_then_caps_then_consumes
  - Rollback: revert the PR. There is deliberately NO runtime switch: the
    2026-10-20 REJECT list bars a hold other than 600 s without a fresh dated
    quote, and a switch that turns it off is such a hold.


### PR C — top_volume spill replay window (storage)

- [x] C1. A spilled hour is set aside by the age of its END and gets a second
  evening; an unparseable spill name no longer holds every partition's archive.
  - Files: crates/storage/src/tick_spill_replay.rs, crates/storage/src/partition_archive.rs
  - Tests: test_spill_file_older_than_counts_from_the_hour_end_and_saturates,
    test_unparseable_spill_name_does_not_hold_every_partition,
    test_undated_top_volume_spill_file_is_set_aside

### PR D — guards and docs (common tests, trading risk, docs)

- [x] D1. Shell budget covers compose CMD-SHELL healthchecks and systemd units outside deploy/systemd.
  - Files: crates/common/tests/shell_budget_guard.rs
  - Tests: compose_healthchecks_are_budgeted, every_systemd_unit_is_scanned
- [x] D2. Coded error on the tick-gap tracker emit; the code guard flags every production `error!` without `code =` (frozen §28 files allowlisted until their own approval).
  - Files: crates/trading/src/risk/tick_gap_tracker.rs, crates/common/tests/error_code_tag_guard.rs
  - Tests: every_production_error_carries_a_code
- [x] D3. CLAUDE.md: O(1) table rows for the frame ring backlog and the silent-swap register; stale benchmark rows; `variables.tf` IOPS text.
  - Files: CLAUDE.md, deploy/aws/terraform/variables.tf
  - Tests: claude_md_codebase_map_guard
  - Done: merged to main in #2117 (frame ring and silent-swap rows, IOPS text, struck bench rows); ticked 2026-10-27.

### PR W4 — confirm parked live acks once they are durable (storage, app) — 2026-10-27 wave

- [x] W4. Parked live acks under latched apply lag (the "Follow-up" below,
  now its own item, owner: "go ahead with next wave ... fix and resolve and
  implement everything", 2026-10-27). While any table is latched untrusted,
  every QuestDB probe is unclean, live acks park, and a session pins its whole
  stretch of WAL until a later replay reaches it. Fix, keep-more only: a
  parked live ack is confirmed (its range trimmed from the unapplied ranges)
  only after a full `WATERMARK_DURABILITY_LAG_SECS` AND two clean probes
  follow it with no suspicion episode beginning in between, the same rule
  `settling/` uses for replays; an episode that begins first keeps the range.
  First measure live (read-only, the `/tickvault/prod/metrics` scrapes):
  `tv_wal_unapplied_pinned_bytes`, `tv_wal_suspect_seconds_total`,
  `tv_wal_suspect_episodes_total` per session, and state whether any session
  had clean probes at all; if none did, say so and confirm only what the rule
  allows (no relaxation of the probe rule).
  - Files: crates/storage/src/wal_applied_watermark.rs, crates/storage/src/wal_unapplied_ranges.rs, CLAUDE.md (the unapplied-ranges row)
  - Tests: test_parked_ack_is_confirmed_after_lag_and_two_clean_probes,
    test_episode_before_confirmation_keeps_the_range,
    proptest_w4_no_frame_is_trimmed_outside_the_rule (all in
    `wal_applied_watermark.rs::w4_parked_confirm_tests`)
  - Done (ticked 2026-11-06): the code is already on main, ported in #2187
    (2026-11-03) as the newer lineage inside `wal_applied_watermark.rs`
    (`ParkedConfirm`, `confirm_parked_if_due`, `ProbeVerdict`,
    `UnappliedRanges::remove_span`, `tv_wal_parked_confirm_total{outcome}`),
    wired from `wal_suspension_watcher.rs` (`note_questdb_probe_verdict`).
    The never-opened `claude/assurance-fix-w4` commit (78087502e, separate
    `wal_parked_confirm.rs` module) is SUPERSEDED and was not cherry-picked:
    it would add a second, conflicting implementation of the same rule
    (two `ProbeVerdict` types, two counter families). Re-reviewed against
    current code; the missing CLAUDE.md O(1) row is added. The live
    measurement in the branch commit (26 Oct: 0 suspicion episodes, 0
    suspect seconds, 225 GB pinned flat) shows W4 does not shrink that
    backlog; what pins it there is Unknown (candidates, not measured: boot
    tails, shed buckets, a migrated backlog that is not draining).

### Follow-up (own plan item, not in this plan)

- PR W growth under latched apply lag: confirm parked live acks once a full
  durability lag (`WATERMARK_DURABILITY_LAG_SECS`) and two clean probes follow
  them, as `settling/` does for replays, so a session that ends while a table
  is latched untrusted stops pinning its whole WAL. Needs its own design (it
  changes what an ack under suspicion is trusted for) and the live
  measurement named in W1 first: `tv_wal_unapplied_pinned_bytes` read
  together with `tv_wal_suspect_seconds_total` and
  `tv_wal_suspect_episodes_total` over several sessions (2026-10-20, PR W
  review round 4). While any table is latched, every probe is unclean, acks
  park, and on a box whose backlog does not drain that is about one session
  of WAL per lag-latched day.
### PR G — holiday gate enabled on deploy, weekday-holiday-only (app, aws-lambdas, common tests, storage tests, deploy workflows, terraform)

Root cause, **Assumed** until `systemctl is-enabled tickvault-holiday-gate` is
read on the box: the deploy copied `tickvault-holiday-gate.service` on every run
but enabled only `tickvault` and `tickvault-host-tuning`; only first-boot
user-data enabled the gate, terraform ignores `user_data` changes, so the live
box ran on the 2026-10-02 and 2026-10-20 NSE holidays. Enabling it as-is would
have stopped every non-trading boot, weekends included (`--check-trading-day`
exit 75 = weekend or holiday): the post-merge catch-up deploy (every 30 minutes,
all seven days), a console start, filesystem recovery, downsize, and a manual
Sunday Muhurat start on 2026-11-08, each with a false "NSE holiday" page.

- [x] G1. The deploy enables all three units in one `systemctl enable` element
  after `systemctl daemon-reload`; the gate unit's copy is unconditional, so a
  missing file fails the deploy instead of silently failing the combined enable.
  The guard helper counts a later `systemctl disable|mask` as removing a unit.
  - Files: .github/workflows/deploy-aws.yml, crates/app/tests/host_tuning_boot_wiring_guard.rs,
    crates/common/tests/shell_budget_guard.rs (deploy-aws 95 / 11,980 → 94 / 11,907,
    run lines 643 → 642 → 640 with G3, shrink only)
  - Tests: deploy_enables_the_holiday_gate_after_daemon_reload,
    holiday_gate_enable_check_bites, ssm_workflow_shell_never_grows,
    workflow_run_lines_never_grow
  - Honest limits: the combined enable keeps `|| true`, so a failed enable is
    not reported by the deploy. A `systemctl is-enabled tickvault-holiday-gate`
    echo in an existing verify element was considered (review, 2026-10-20) and
    NOT added: the deploy-aws.yml SSM payload budget (94 elements / 11,907
    bytes) was measured exactly with no slack, so any echo grows it, and the
    budget only shrinks. The enabled state stays unverified by the deploy; read
    it on the box (the root-cause check above).
- [x] G2. The gate stops the box ONLY on a Monday-to-Friday NSE holiday with no
  Muhurat or mock session, at a boot inside 08:25 to 09:15 IST (the 08:30 IST
  EventBridge start); every other boot (weekend, Muhurat, trading day, outside
  the window, unreadable config) starts the app with one log line saying why.
  Marker and SNS page unchanged for the stop. `--check-trading-day` unchanged.
  The window START is `shutdown_class::HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST`
  itself (08:25, no copy), which is also the `market_open_readiness` Lambda's
  holiday self-stop floor (`HOLIDAY_SELF_STOP_EARLIEST_IST_MINUTES`, pinned by a
  source-scan test). `shutdown_class`'s quiet window END moves 09:00 → 09:25 =
  the gate's 09:15 end + `HOLIDAY_GATE_STOP_LAG_SECS` (600 s: the gate unit's
  300 s outer bound + 300 s **Assumed** for EC2 to signal the app), so every gate
  stop classifies `ScheduledStop`; a compile-time assert holds gate window ⊆
  quiet window with equal starts. (Review fix 2026-10-20: the first draft started
  the gate at 08:00, so a gate stop 08:00–08:25 paged as an external stop and the
  readiness Lambda read it as "box not running".)
  - Files: crates/app/src/holiday_gate.rs, crates/app/src/shutdown_class.rs,
    crates/app/src/main.rs, crates/app/src/boot_helpers.rs,
    crates/aws-lambdas/src/market_open_readiness.rs (doc pin only),
    crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: test_weekday_holiday_inside_the_scheduled_start_window_stops,
    test_weekday_holiday_outside_the_window_starts, test_weekend_boots_start_at_any_hour,
    test_sunday_muhurat_starts, test_trading_day_starts_inside_and_outside_the_window,
    test_unreadable_calendar_fails_open_even_at_the_scheduled_start,
    test_classify_calendar_day, test_scheduled_start_window_is_0825_to_0915_ist_around_the_0830_start,
    holiday_gate_window_start_matches_the_readiness_lambdas_floor,
    holiday_gate_unit_timeout_covers_the_marker_retries,
    test_classify_shutdown_at_the_holiday_gates_latest_stop_is_scheduled,
    test_classify_shutdown_holiday_gate_window_consts_are_0825_and_0925_ist,
    test_classify_shutdown_holiday_gate_self_stop_window_only,
    test_trading_day_gate_reads_the_same_layered_config_as_the_boot,
    holiday_gate_is_wired_and_fail_open,
    test_decide_second_start_lets_only_a_same_day_restart_through,
    test_marker_put_is_tried_twice_and_bounded, test_names_match_the_script
  - Review fixes (2026-10-20, second round): (a) a deliberate restart inside
    08:25 to 09:15 IST on a holiday-stop day was stopped again and paged again.
    Before it stops, the gate now reads its own `/tickvault/<env>/holiday-stop-date`
    (`read_holiday_marker`, 30 s ceiling, `ParameterNotFound` = absent) and the
    pure `decide_second_start` lets the boot start with one log line when the
    marker is today's IST date; absent, an older date, empty, or a FAILED read
    stop as before (reviewer's choice: a failed read cannot tell the first boot
    from the second, and the first is the common case). The page now says a
    second start today stays on, and says so only when today's stop was
    recorded. (b) The marker put is retried once after 2 s
    (`MARKER_PUT_ATTEMPTS` = 2); with the read, the stop path grows by at most
    62 s, inside the unit's 300 s `TimeoutStartSec`. (c) The readiness Lambda's
    08:25 floor doc now says "launched at or after about 08:25" and names the
    few-seconds skew between the EC2 launch time it reads and the gate's own
    clock.
  - Honest limits: a deploy or manual start that lands inside 08:25 to 09:15 IST
    on a weekday holiday, BEFORE the gate stopped the box that day, is stopped
    like the scheduled start; the `/opt/tickvault/ALLOW_HOLIDAY_RUN` marker
    overrides it. A manual or budget stop on a non-trading day between 08:25 and
    09:25 IST now renders quiet (was 08:25–09:00). A same-day reboot of a box
    whose stop CALL failed now starts the app instead of trying the stop again.
    After a second start the marker still reads today, so the start-watchdog,
    the deploy-watchdog and the market-hours alarm gate treat the running box as
    holiday-stopped for the rest of the day (no self-start, no automatic
    redeploy, no liveness page; `deploy-aws.yml` run by hand still deploys a
    running box, and will not start it again if it stops). When both marker puts fail, a second start
    the same day is stopped again (errs toward the stop). A launch in the last
    seconds before 08:25 whose gate runs after 08:25 is stopped but read by the
    readiness Lambda as launched before its floor, so it pages "box not
    running".
- [x] G3. The deploy chain never restarts a box the gate stopped. The
  deploy-watchdog Lambda (instance-start event, 08:50 / 15:51 IST crons) reads
  `/tickvault/<env>/holiday-stop-date` through the start-watchdog's
  `holiday_stop_is_today` (same parameter, IST date, fail-open) after a stale
  verdict and does not dispatch when it is today: one `info!` line, result
  `"skipped": "holiday_stop"`, no page (it has no counters; the mismatch metric
  is still published). Terraform grants it `ssm:GetParameter` on exactly that
  parameter and sets `HOLIDAY_STOP_PARAM`. `deploy-aws.yml` reads the same
  marker beside the budget-stop latch, before `start-instances`, and skips green
  (`DEPLOY_SKIP_REASON=holiday_stop`); `deploy-aws-after-close.yml` and
  `postmerge-catchup.yml` both end in that check (comments only there). The 4
  new run lines are paid by merging two 4-line state branches (run lines
  642 → 640; SSM payload and awk/jq budgets untouched).
  - Files: crates/aws-lambdas/src/deploy_watchdog.rs,
    crates/aws-lambdas/src/start_watchdog.rs (`SdkSsm.client` `pub(crate)`),
    deploy/aws/terraform/deploy-watchdog-lambda.tf, .github/workflows/deploy-aws.yml,
    .github/workflows/deploy-aws-after-close.yml, .github/workflows/postmerge-catchup.yml,
    crates/common/tests/shell_budget_guard.rs, crates/storage/tests/aws_deploy_safety_guard.rs
  - Tests: handle_reads_the_holiday_stop_marker_before_it_dispatches,
    holiday_marker_blocks_only_on_todays_ist_date (in start_watchdog.rs since
    the second review round, next to the `run`-driven holiday-marker checks; it
    tests `start_watchdog::holiday_stop_is_today`),
    deploy_chain_does_not_restart_a_box_the_holiday_gate_stopped (its
    deploy-watchdog check is scoped to `handle`'s body, so the Lambda's own test
    module literal cannot satisfy it),
    deploy_watchdog_handle_scope_bites_on_a_test_module_literal,
    workflow_run_lines_never_grow
  - The `deploy-aws.yml` skip notice now says: start the box from the console
    (a second start the same day is let through), then re-run with
    `confirm_market_hours=yes` (09:00-15:45 IST) or after 15:45 IST. Same line,
    run-line budget unchanged.
  - Honest limits: on the instance-start event the watchdog can fire before the
    gate writes the marker in that same boot, so that one firing can still
    dispatch; the deploy it starts skips at the marker once the box is stopped,
    or meets the stop during its verify (reported as an external stop, not a
    crash). A holiday-skip run counts as covering its SHA for
    `postmerge-catchup`, so that SHA's redeploy is the watchdog's on the next
    weekday boot. An unreadable marker deploys (fail-open, as the budget latch).
    The terraform grant needs an apply before the Lambda can read the marker;
    until then the read fails open and the watchdog dispatches as before.
    A merge on a holiday-stop day is NOT deployed that day: the next weekday
    08:30 boot runs the stale binary, the deploy-watchdog (instance-start
    event, 08:50 IST cron) reports it stale (`binary-sha-stale` metric) and
    dispatches the redeploy inside the 08:30-09:00 prep window. The watchdog's
    skip ignores box state: it reads only the marker, so after a manual start
    on a holiday-stop day (let through by the gate's second-start rule, or
    after 09:15 IST) a stale binary is not auto-deployed that day; re-run
    `deploy-aws.yml` by hand (a running box deploys).

### PR J — 2026-10-26 assurance-audit fixes (storage, app, core, api, common tests)

Approved by Parthiban, 2026-10-26: "go ahead fix everything fully parallel dude".
Source: the 2026-10-26 seven-dimension audit plus the WebSocket-continuity
attack (adversarially verified). Implemented in parallel on sub-branches
`claude/assurance-fix-g1..g5`, merged into one draft PR.

- [x] J1 (storage) Tick and depth rescue deferral checks the WAL flushed-through
  sequence (`wal_frame_fate::flushed_high_seq`) before trusting the WAL; a frame
  the WAL has not flushed goes to the inline spill instead. Correct the
  `no_segment` comment in `ws_frame_spill.rs`. Contract labels sanitised once at
  publish, not per tick row. Hot-path telemetry slots cache-line padded.
  - Files: crates/storage/src/tick_persistence.rs, crates/storage/src/depth_persistence.rs, crates/storage/src/ws_frame_spill.rs, crates/storage/src/hot_path_telemetry.rs
  - Tests: storage unit tests for the deferral decision; dhat_flush_buffer_recycle unchanged
- [x] J2 (app) The frame drain's shutdown arm drains the ring and counts the
  frames still queued (WAL-backed as deferred, unbacked as lost, coded
  WS-SPILL-02); rescue/offload shutdown abandonment counts ROWS on
  `tv_ticks_lost_total{source="rescue_abandoned"}`.
  - Files: crates/app/src/dhan_feed_stack.rs
  - Tests: app unit tests for the shutdown drain count
- [x] J3 (app, storage tests) `instrument_lifecycle` is ensured in production
  beside its audit twin and the write is held when the ensure is refused;
  order-side audit ensures retry until keyed; AUDIT-WS-01 drop warn carries a
  code and is latched once per episode; a guard requires DEDUP UPSERT KEYS on
  every CREATE TABLE literal (dated allowlist for the two scratch tables).
  - Files: crates/app/src/order_observability.rs, crates/app/src/dhan_rest_stack.rs, crates/storage/tests/
  - Note (2026-10-28): the J3 lifecycle half lived in the app lifecycle writer module, DELETED by owner Quote 44 with the daily lifecycle write (PR N below); the J3 ensure-and-hold pin was replaced by the no-writer guard.
  - Tests: ensure-wiring source guard; every_create_table_carries_a_dedup_key guard
- [x] J4 (core, api) Order-update WebSocket connects with explicit bounded
  limits; the connect-config guard scans every `connect_async*` site; the error
  summary writer streams and bounds its read; the ping/pong counter uses
  pre-resolved handles; `/api/debug/logs/jsonl/latest` is size-capped.
  - Files: crates/core/src/websocket/order_update_connection.rs, crates/core/src/websocket/connection.rs, crates/core/src/notification/summary_writer.rs, crates/api/src/handlers/debug.rs
  - Tests: config limit unit test; guard extension; summary writer bounded-read test; debug cap test
- [x] J5 (common tests, scripts, CI) Rust-only guard holes closed: interpreter
  aliases, case-insensitive `<script`, JS byte ceiling, `javascript:` URLs,
  banned frontend extensions and SVG script scan, Rust SSM-sender discovery,
  workflow line-continuation join; `brew install` removed; unversioned
  `cargo install` / `tool:` rejected and pinned.
  - Files: crates/common/tests/rust_only_guard.rs, crates/common/tests/shell_budget_guard.rs, crates/common/tests/browser_surface_and_toolchain_guard.rs, scripts/setup-observability.sh, .github/workflows/ci.yml
  - Tests: a self-test row per new token / extension / discovery rule
- [x] J6 (core) Dhan's documented `expiryTime` shape with milliseconds
  ("2026-01-01T00:00:00.000") is parsed instead of silently falling back to
  now + 24 h.
  - Files: crates/core/src/auth/types.rs
  - Tests: test_parse_expiry_time_documented_millisecond_string_is_parsed_exactly
- [x] J8 (aws-lambdas, terraform) Capture the NSE Muhurat session on its date
  (owner, 2026-10-26: "yes capture muhurat"; 2026: Sunday 2026-11-08). The box
  runs Mon-Fri only, so on a weekend Muhurat nothing starts it and the hourly
  curfew and hard-stop guard would stop a manual start. New pure module
  `muhurat_window` (box up from session start - 60 min to session end + 60
  min); terraform `var.muhurat_sessions` (dates must equal
  `[[trading.muhurat_trading_dates]]`, times "HH:MM" IST, EMPTY until the owner
  fills them from NSE's circular) rendered to `MUHURAT_SESSIONS` on the start
  watchdog and the hard-stop guard; one DATED EventBridge rule per date
  (`cron(0/15 0-18 D M ? YYYY)`, never a weekly weekend cron) running the new
  `muhurat_check` mode: start the box inside the window (budget-stop latch and
  holiday marker honoured, one page on a failed first attempt), stop the box it
  started after the window (keep-alive and drain hold honoured), and with empty
  or unreadable times log `LAMBDA-CONFIG-01` and never start (fail closed). The
  curfew and the hard-stop guard treat the window as in-hours (the guard still
  runs its in-window budget check). Budget kill line unchanged.
  - Files: crates/aws-lambdas/src/muhurat_window.rs, crates/aws-lambdas/src/start_watchdog.rs, crates/aws-lambdas/src/hard_stop_guard.rs, crates/aws-lambdas/src/lib.rs, crates/aws-lambdas/tests/muhurat_window_wiring_guard.rs, crates/aws-lambdas/tests/drain_hold_wiring_guard.rs, deploy/aws/terraform/variables.tf, deploy/aws/terraform/start-watchdog-lambda.tf, deploy/aws/terraform/budget-guards.tf
  - Tests: muhurat_window::tests (14), start_watchdog test_muhurat_check_* (8) + test_muhurat_after_window_should_stop_table + test_curfew_never_stops_the_box_inside_a_muhurat_window, hard_stop_guard test_muhurat_window_is_in_window_and_never_schedule_stops + test_muhurat_timing_unset_keeps_the_out_of_window_stop, muhurat_window_wiring_guard (5)
  - Not in J8 (Risk, follow-ups): the candle fold accepts 09:15-15:40 only, so an
    EVENING session gets ticks and depth rows but no candles (an afternoon
    session inside that range is folded); the post-merge and catch-up deploys
    can restart the app on a weekend (deploy-aws.yml blocks Mon-Fri 09:00-15:45
    only), so no merge should land during the session; the data-silence redial
    stays off on a non-trading day unless 3+ sockets are streaming.

### PR K — 2026-10-26 wave-2 audit fixes (storage spill + WAL, app drain hold, core silence spacing)

Approved under the same session quotes as PR J ("go ahead fix everything fully
parallel dude", "fix and reosleve and implement evrythign dude ... im ggiving you
all teh permissiosn"). Each item closes a gap the wave-1 slice audits and the
WebSocket-continuity attack confirmed in source; none changes an operator-locked
rule (808 park, $75 stop, token self-heal and frame-silence `act` stay as they are).

- [x] K1 (storage, app) Spill replay and spill files: only HTTP 400/413/422 count
  as a bad payload (other 4xx are retried like a 5xx); a round stops after 3 files
  in a row are quarantined with the same status; the bisection POST cap follows
  the 64-line cap; a whole framed depth record that QuestDB refuses increments the
  paged rejected-lines counter; a spill append first writes a newline when the
  file's last byte is not one; the shared QuestDB client ignores proxy variables;
  at shutdown the parked rescue hand-off uses a deadline, then spills inline or
  counts the rest abandoned; the drain hold also counts depth framed spill files.
  - Files: crates/storage/src/tick_spill_replay.rs, crates/storage/src/depth_spill_frame.rs, crates/storage/src/tick_persistence.rs, crates/storage/src/depth_persistence.rs, crates/storage/src/http_client.rs, crates/app/src/drain_hold.rs
  - Tests: one regression test per change (status classification, quarantine streak stop, POST cap, set-aside counter, newline guard, park deadline, depth folder counted)
- [x] K2 (storage) WAL core: a failed segment sync raises the lost mark so the
  rescue stops deferring to it, logged `error!` with its code; a shutdown that
  abandons a stuck writer is counted and never logged "complete"; a segment that
  cannot be read (I/O error) stays in place with a retry cap instead of being
  archived unread; a failed listing of `replaying/` refuses the pass; staging
  failures are coded and counted; a discarded segment is synced and cleared from
  the open registry; the queue byte cap is resolved at construction, not on the
  first frame; the stale re-capture comment is corrected.
  - Files: crates/storage/src/ws_frame_spill.rs, crates/storage/src/wal_replay_settle.rs
  - Tests: one regression test per change
- [x] K3 (core, app) The 300 s silence fallback redials at most one socket per
  kind per `CONFIRMED_REDIAL_SPACING_SECS`, so a market-wide silence no longer
  redials every main-feed socket in the same second; the tick-gap detector's
  LTT-backward and volume-jump verdicts are counted (pre-resolved counters, local
  `/metrics` only) instead of discarded.
  - Files: crates/core/src/websocket/pool_supervisor.rs, crates/core/src/pipeline/tick_gap_detector.rs, crates/app/src/dhan_feed_stack.rs
  - Tests: five silent sockets for 1,800 s never redial two inside one spacing; verdict counters move on a crafted backward LTT and volume jump; DHAT on the observe path unchanged

- [x] K4 (app, storage) Daily tick conservation and crash gaps: after the
  session, compare the packets the drain folded today (per feed) with the rows
  in `ticks` for today minus counted losses and deferrals, and write one
  `tick_conservation_audit` row (DEDUP `ts, trading_date_ist, feed`); a
  mismatch logs a coded `error!` (log only: no alarm, filter or page); an
  unreadable count is `unverified`, never a false mismatch. At boot inside the
  session window, the downtime since the previous process's last WAL receipt
  is written as one `feed_gap_audit` row (`kind = process_down`).
  - Files: crates/app/src/tick_conservation.rs (new), crates/app/src/main.rs, crates/storage/src/tick_persistence.rs, crates/storage/src/ws_frame_spill.rs
  - Tests: verdict unit tests (match, mismatch, unverified), DDL carries DEDUP with feed, boot gap row only inside the window

- [x] K5 (storage, app) Real p99 per hot-path stage (audit J7, owner "Always
  achieve O(1) even with p99 everywhere"): from the existing fixed atomic stage
  histograms, compute p50/p99/p99.9 once a second on the `tv-telemetry` thread
  (O(stages x buckets), never on the frame path), publish
  `tv_hot_path_stage_p99_ns{stage}` (and p50, p999) on local `/metrics` only,
  and log one coded `warn!` per episode when a stage's p99 passes its budget;
  plus a Criterion bench reading p99 from samples for the drain stage.
  - Files: crates/storage/src/hot_path_telemetry.rs, crates/app/benches/ (existing bench file for the drain)
  - Tests: quantile maths against a known distribution (empty, one bucket, top bucket), DHAT on record_stage_nanos unchanged, episode latch once

- [x] K6 (app, storage, quality) Drain p99 tail (owner, 2026-10-26: "Always
  achieve O(1) even with p99 everywhere"). K5 measured the main-feed drain
  bench at p50 0.9 us/tick, p99 3.2 us/tick (budget 2.5), p99.9 14.5 us/tick,
  max 0.48 ms on this container. Find where the tail comes from (profile the
  bench; Assumed lead: a writer row buffer growing inside a timed frame) and
  remove it at its source (pre-size or move off the timed path), keeping
  per-tick O(1) and zero allocation (DHAT gates stay green). Add one
  hot-path stage for the drain's per-frame work (decode + fold + append),
  recorded with `record_stage_nanos` (O(1), no allocation) so the live p99
  covers it; a p99 over budget fails the bench run as well as a median
  (checked in Rust; shell lines do not grow, rust-only lock).
  - Files: crates/app/src/dhan_feed_stack.rs, crates/storage/src/hot_path_telemetry.rs, crates/storage/src/tick_persistence.rs, crates/app/benches/feed_drain.rs, quality/benchmark-budgets.toml
  - Tests: DHAT on the drain unchanged with the new stage; stage recorded once per frame; the p99 check fails on a synthetic over-budget sample (self-test); before/after p99 numbers in the PR body
  - Done 2026-10-26: the tick writer's row buffer is pre-sized (2 MiB), a `drain_main_frame` stage (1 ms budget) times decode + fold + append, and the feed_drain bench fails on a p99 over budget (self-tested). Bench p99 per frame 9,035 -> 6,940 ns (-23%), p99.9 35,095 -> 28,448 ns; drain-seam DHAT 11 blocks -> 0. Most of the remaining tail is the test VM's scheduling noise (a no-op loop shows the same p99/p50 ratio). The CI runner's p99 is not measured yet (Assumed headroom 6,250 ns/tick).

### PR L — 2026-10-26 wave-3 audit fixes (core socket, storage seal chain, trading fold, app lane, aws-lambdas, workflows, terraform)

Approved under the same session quotes as PR J/K ("fix and reosleve and implement
evrythign dude ... im ggiving you all teh permissiosn", "dude ensure to fox
reosleve and impelment evryhtign dude okay?"). Each item closes a finding the
wave-3 slice audits (W2-a socket, W2-b stop/restart/token, W2-c candle chain)
confirmed in source. No operator-locked rule changes: 805/808 policy, the $75
line, the frame-silence `act` default and the shell budget stay as they are.

- [x] L1 (core) Socket read path: a write that fails with no socket decision
  first takes any owed disconnect code (`pending_close`) and classifies it, and
  the close drain returns the first disconnect code it reads (lone or stacked)
  to `classify_disconnect`; a close whose send stalls still drains the frames
  already readable without waiting; a closed ring is counted under its own
  reason, not `ring_full`; the main-feed packet-cap check matches the depth walk
  (`>`); the boot token probe mints only on a real auth failure (network, 429,
  5xx and parse errors go through `classify_failure_reason`).
  - Files: crates/core/src/websocket/connection.rs, crates/core/src/websocket/pool_supervisor.rs, crates/core/src/parser/dispatcher.rs, crates/core/src/auth/token_manager.rs
  - Tests: scripted socket where the write ticket fails while a stacked 805 is owed (805 parks); stalled-close drain keeps buffered frames; closed-ring label; 70,000-packet cap; probe error classes
- [x] L2 (storage, app) Seal chain at shutdown and on disk: the candle sender
  sets `request_min_throughput` so one failed flush cycle fits the shutdown
  budget; after the first failed flush the final drain spills the rest without
  flushing again; a shutdown timeout keeps the crash marker not-clean with the
  real unwritten count; the producer-side escalation honours the 4 GiB seal disk
  floor (counted refusal); a replayed copy no fuller than the committed one is
  skipped (`<=`) and a pinned ledger overflow clears; a spill handle closed by
  staging or a day switch is synced or parked, never dropped unsynced.
  - Files: crates/storage/src/shadow_candle_writer.rs, crates/storage/src/seal_writer_loop.rs, crates/storage/src/seal_writer_runner.rs, crates/storage/src/seal_spill_ledger.rs, crates/storage/src/seal_spill.rs, crates/app/src/main.rs
  - Tests: one regression test per change (hung-QuestDB final drain bound, marker not-clean on timeout, floor refusal counted, equal-fullness replay skipped, rotated handle synced)
- [x] L3 (trading, app) Candle fold and live lane: a packet the fold calls stale
  can widen high/low but never moves close, close time, OI or quantities; on a
  Muhurat day the fold's session gate follows the same Muhurat window the tick
  writer uses; the seal-accounting `debug_assert` counts rescued seals; the
  connected-but-silent check (RISK-GAP-03) runs on a Muhurat day; a lane refused
  for a missing token manager or an unheld instance lock retries on a bounded
  backoff instead of returning for the rest of the process.
  - Files: crates/trading/src/candles/multi_tf_aggregator.rs, crates/trading/src/candles/aggregator_cell.rs, crates/app/src/dhan_feed_stack.rs, crates/app/src/dhan_rest_stack.rs
  - Tests: stale packet keeps close; Muhurat-day tick at 18:05 folds into a 1m bar; rescued seal does not trip the assert; lane retries then dials; restart differential unchanged
  - Done 2026-10-26 except one part: the REST stack still PARKS after its 300 s lock patience. A 60 s re-try was written and reverted, because the dual-instance lock (§3.5) parks on purpose so a second box cannot take the lock when the owner restarts; changing it needs the owner's dated quote in that file first.
- [x] L4 (aws-lambdas, app, workflows, terraform) Stop paths in the capture
  window: every workflow market-hours guard uses 09:00–15:40 IST (aws-control,
  downsize-instance) and is pinned by a test; the deploy failure auto-stop and
  `terraform apply` re-check IST right before acting; the holiday gate's stop
  arm ends at 09:00; October's spend is distrusted for the first 48 h of a UTC
  month; a Muhurat date with unset or malformed times is a Lambda error (the
  existing Errors alarm fires) and terraform checks the time range and start <
  end; the daily stop and stop check skip a Muhurat window; the liveness alarm
  disarms at 15:40 and is armed on a Muhurat date; the stale "recover manually"
  unit comment is corrected. Workflow shell lines do not grow (shell budget).
  - Files: .github/workflows/aws-control.yml, .github/workflows/downsize-instance.yml, .github/workflows/deploy-aws.yml, .github/workflows/terraform-apply.yml, crates/app/src/holiday_gate.rs, crates/aws-lambdas/src/budget_spend.rs, crates/aws-lambdas/src/start_watchdog.rs, deploy/aws/terraform/variables.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, deploy/systemd/tickvault.service
  - Tests: window pins for every guarded workflow; budget day-2 stale reading untrusted; Muhurat config error returns Err; daily stop skips the window; holiday gate at 09:05 starts
  - Done 2026-10-26 except arming the liveness alarm on the Muhurat date: before the first tick its no-ticks alarm reads the app's uptime, so arming at the configured session start (13:30, before the reported 14:05 pre-open) would page at once; needs NSE's official times and a start offset. `terraform fmt` not run (not installed).

### PR M — 2026-10-26 live-session findings (storage, app, core, trading, terraform)

From a read-only check of the live AWS account for the 2026-10-26 session
(CloudWatch metrics scrapes, app logs, EBS and EC2 metrics). Approved under
the same session quotes as PR J/K/L plus "do the slonwess test directl yfrom
liv e aws" and "do a live aws check ... live access evryhtign" (2026-10-26).
Measured: 0 ticks lost and 0 socket disconnects all day; every hot-path stage
inside its budget at p99; but the root disk sat at its 125 MiB/s ceiling from
09:30 to 15:40 IST (CPU about 5%), the tick writer fell behind for about 6 h
(9.2M rows rescued to spill, all replayed, 0 failed), the WAL backlog held
225 GB flat, the 1-minute cross-check read `diverged` (16.6% of price
fields, 10,186 live minutes missing, 45 traded) and the timeframe check found
48,581 mismatches and 27,680 missing rows.

- [x] M1 (storage, app) Disk write amplification: the box writes about
  110-190 MiB/s in session (500 MiB/s days) against about 1 MB/s of tick
  rows. Measure which writer it is (QuestDB out-of-order merges from spill
  replay or WAL catch-up, the frame WAL, candle tables, uploads) from the
  live metrics, and remove the amplification at its source so a session
  fits well under 125 MiB/s. Report before/after bytes per tick.
  - Files: crates/storage/src/tick_spill_replay.rs, crates/storage/src/tick_persistence.rs, crates/storage/src/ws_frame_spill.rs, crates/app/src/dhan_feed_stack.rs
  - Tests: one regression test per change; a measured bytes-written figure in the PR body
  - Done 2026-10-26 in part: tick spill replay now waits until after 15:40 IST unless the spill volume runs low (no row dropped; the evening drain hold covers it), and every `ticks` row is counted by where its timestamp lands (`tv_ticks_ts_placement_total`). Measured 51 KB of disk writes per tick (116 MiB/s for about 2,390 ticks/s). Main cause found and NOT fixed: `ticks` is partitioned on the exchange trade time and 70-75% of rows are repeat quotes carrying an old trade time, so about 5% land in an already closed hour partition and most commits force QuestDB to rewrite partitions (QuestDB internals Assumed). The fix changes the data-integrity rule for `ts`, so it is listed for the owner below.
- [x] M2 (storage) WAL backlog never drains: 1,696 pinned segments (225 GB)
  flat all day, boot catch-up defers 1,759 segments every day on its RAM
  budget. Find why no catch-up makes net progress and make each boot or
  evening drain shrink the backlog (plan item W3 shape: settle per batch),
  bounded and O(1) per frame.
  - Files: crates/storage/src/wal_unapplied_ranges.rs, crates/storage/src/ws_frame_spill.rs, crates/storage/src/wal_replay_settle.rs, crates/app/src/evening_drain.rs
  - Tests: lifetime property test where the backlog shrinks across boots; regression tests per change
  - Done (cd658ed): three causes fixed. (1) The replay pacer read a row count `flush_ticks` zeroes even when a full queue gives rows back, so it never waited (`pace_waits = 0` on all 8 boot replays 19-26 Oct); it now reads the writer's retained rows (`replay_rows_held`, O(1)). (2) A catch-up round settled all or nothing; it now folds, acks and settles one segment at a time (`WalReplayBatch::segment_ends`, `SettleState::settle_named`, `dhan_feed_stack/replay_chunks.rs`). (3) `evening_step` gave up at 15:40 and latched; it now waits 12:00-15:50. Tests: `test_regression_m2_replay_pacer_sees_rows_a_full_queue_gave_back`, `test_regression_m2_lifetime_backlog_shrinks_to_zero_and_never_retires_unlanded` (12 seeds; a segment is archived only if its rows landed in that process with no suspicion episode after), `test_regression_evening_step_waits_before_the_window_instead_of_latching_give_up`; each checked to fail without its fix. Except: the boot pass still settles whole (falls back to per-segment at catch-up); the per-boot frame cap (3,000,000) is unchanged, so the 225 GB drains in about 50 trading days with the evening restarts (Assumed), over a year without them; not yet seen on the live box.
- [x] M3 (app, trading) Data checks failed: find whether the 16.6% 1-minute
  divergence, the 10,186 missing live minutes and the 48,581 timeframe
  mismatches come from rows not yet applied when the check read (writer lag)
  or from the fold, and fix the cause (the check must not read before the
  rows it judges are applied; a fold bug gets its regression test).
  - Files: crates/app/src/dhan_live_crossverify_boot.rs, crates/app/src/dhan_live_crossverify.rs, crates/app/src/tf_consistency_boot.rs, crates/trading/src/candles/multi_tf_aggregator.rs
  - Tests: regression test per cause found; readiness barrier covers the spill/replay backlog
  - Done 2026-10-26: the timeframe check waits (until 15:55 IST at most) for sealed, saved and applied candles and counts a bar still unsealed as `unsealed_at_read`, not missing; the two shapes the fold makes by design (a day extreme moving across a minute boundary, the 09:15 bar's first volume) are counted apart and no longer logged as TF-VERIFY-01 (the code was never paged). The 1-minute check's 15-17% divergence is the vendor's full trade tape against our sampled feed (same every day, 13-26 Oct); its summary now logs the split per field. No paging threshold changed.
- [x] M4 (core, app, terraform) Smaller live findings: the socket hand-off
  stage went over its 100 us budget 8,843 times (p99.9 262 us at the open)
  — find and remove the tail at its source, O(1), zero allocation; the
  planned-connections gauge stays at 1 after five sockets come up; the
  contract-to-underlying map refuses 60,988 of 85,988 legs at its 25,000 cap
  every attempt until 09:30 (find whether any subscribed or ranked contract
  loses its underlying, fix if so); two QuestDB console Lambda log groups
  keep logs forever (set retention).
  - Files: crates/core/src/websocket/pool_supervisor.rs, crates/storage/src/ws_frame_spill.rs, crates/app/src/dhan_feed_stack.rs, crates/app/src/contract_underlying_map.rs, deploy/aws/terraform/
  - Tests: DHAT on the socket path unchanged; regression test per change
  - Done 2026-10-26: the WAL writer polls for 50 ms after its last record (1 ms sleep) before its blocking wait, so the reader no longer pays a futex wake per frame (harness: WAL hand-off max 4.0 ms -> 22 us, 0 over budget; frames reach the WAL file up to about 1 ms later); the reserved-connections gauge follows the widen; contract-map refusals are counted only for subscribed contracts (none lose their underlying); the two QuestDB console log groups get 14-day retention in terraform (not applied, not validated locally). Live confirmation waits for the next session.

- [x] M5 (done: `tick_quote_route.rs`, `tick_quotes_persistence.rs`, branch claude/assurance-fix-m5) (storage, app) Repeat quotes to `tick_quotes` (owner Quote 41,
  2026-10-27; scope-lock section "2026-10-27 — REPEAT QUOTES GET THEIR OWN
  TABLE"). Route a main-feed packet with no new trade to a new `tick_quotes`
  table stamped with arrival time (IST), keeping the vendor trade time in
  `exchange_timestamp`; `ticks` unchanged for every other packet. Same writer,
  WAL, rescue, spill, replay (a replayed packet reaches the same table), DEDUP
  `(ts, security_id, segment, capture_seq, feed)`, self-heal ensure, retention,
  partition archive and S3 copy; readers of `ticks` that need quotes (quote
  endpoint, tick conservation K4, cross-checks) read both or are named. O(1)
  per packet (the no-new-trade test reuses the fold's existing per-key state;
  no extra lookup), zero allocation (DHAT). `tv_ticks_ts_placement_total`
  should show the late share of `ticks` rows fall to near 0.
  - Files: crates/storage/src/tick_persistence.rs, crates/app/src/dhan_feed_stack.rs,
    crates/storage/src/partition_manager.rs / partition_archive.rs (retention),
    crates/storage/src/tick_conservation_ledger.rs, crates/app/src/tick_conservation.rs,
    crates/api/src/handlers/quote.rs, CLAUDE.md, docs (table list)
  - Tests (to write): test_regression_repeat_quote_goes_to_tick_quotes_with_arrival_ts,
    test_new_trade_stays_in_ticks_with_exchange_ts, test_replay_routes_like_live,
    test_tick_quotes_dedup_key_includes_feed_and_segment, DHAT on the drain unchanged
- [ ] M6 (app) Evening drain restarts replay up to 5 × `WAL_REPLAY_MAX_FRAMES_PER_BOOT`
  (owner Quote 41): only a boot `evening_drain` recorded as its own planned
  restart; morning and every other boot keep 3,000,000; disk floor, 15-minute
  clock, 3 restarts and 40-minute spacing unchanged.
  - Files: crates/app/src/evening_drain.rs, crates/app/src/main.rs or the
    catch-up budget site, crates/storage/src/ws_frame_spill.rs (if the cap is a const there)
  - Tests (to write): test_regression_evening_restart_gets_five_times_the_frame_cap,
    test_morning_boot_keeps_the_frame_cap, test_unreadable_state_keeps_the_frame_cap

- [x] X1 (done: `wipe_2026_10_31.rs`, `wipe-2026-10-31.tf`, PR #2128 + branch claude/assurance-fix-x1b) (app, terraform) Quote 42: the wipe runs tonight. `wipe_2026_10_31`:
  apply window opens 2026-10-27 18:00 IST (end unchanged), data cutoff
  2026-10-28, S3 scope adds the market-data tables under
  `rebuild-2026-10-22/` (keep-list tables there kept) and the three
  quarantine prefixes, old-root snapshot delete refused before 2026-10-28
  16:30 IST. Terraform role `tv-prod-wipe-2026-10-31`: deletes from
  2026-10-27T12:30Z, dated-key conditions before 2026-10-28, the new
  prefixes, snapshot delete from 2026-10-28T11:00Z, expiry unchanged.
  - Files: (originally crates/app/src/wipe_2026_10_31.rs and crates/app/src/bin/wipe_2026_10_31.rs; both deleted 2026-10-29 after the 28 Oct run, commit 72f55c66b, with the role file and workflow below)
    crates/app/src/wipe_box.rs (cutoff), deploy/aws/terraform/wipe-2026-10-31.tf,
    .github/workflows/wipe-2026-10-31.yml (text only, budgets unchanged)
  - Tests: the tool's date and verdict tests moved to the new dates, plus
    test_regression_wipe_never_touches_data_dated_2026_10_28_or_later,
    test_snapshot_delete_refused_before_wednesday_close,
    test_rebuild_prefix_keeps_keep_list_tables, and the terraform cross-check

### PR N — owner Quote 44: stop the daily instrument_lifecycle write; daily instrument snapshots to the cold bucket (app, storage, docs) — 2026-10-28

Approved by: owner Quote 44 (2026-10-28), recorded in the daily-universe rule file (full text) before any code.

- [x] N1 (app, storage) Stop the daily `instrument_lifecycle` /
  `instrument_lifecycle_audit` write; delete the writer and the storage
  helpers only it used; both tables stay on every keep list, frozen.
  - Files: crates/app/src/dhan_universe.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/storage/src/instrument_lifecycle_persistence.rs, crates/storage/src/lib.rs, crates/storage/src/lifecycle_reconciler.rs
  - Tests: no_production_code_writes_the_frozen_lifecycle_tables, no_production_code_in_app_or_storage_names_a_route_into_the_frozen_tables, strip_cfg_test_items_keeps_code_after_a_test_module, no_questdb_write_runs_before_the_mapping_artifact
- [x] N2 (app) Copy each morning's dated artifact files to
  `instrument-snapshots/<date>/` in the cold bucket, verified, off the boot
  path, one bounded pass at a time with a queued rerun; the 7-day sweep deletes
  a file (and a `.json.tmp` leftover) only after a verified copy exists.
  - Files: crates/app/src/instrument_snapshot_upload.rs, crates/app/src/dhan_universe.rs, crates/app/src/main.rs, crates/app/tests/universe_boot_race_guard.rs
  - Tests: test_run_snapshot_pass_copies_verifies_and_marks_then_skips_without_network, test_run_snapshot_pass_sends_different_bytes_to_a_sidecar_never_over_the_first, test_run_bounded_snapshot_pass_ends_a_hung_pass_as_a_counted_failure, test_run_snapshot_pass_stops_after_three_s3_timeouts_in_a_row, test_drive_snapshot_passes_runs_once_more_for_a_request_during_a_pass, test_claim_or_queue_pass_queues_a_rerun_and_counts_the_skip, test_sweep_stale_artifacts_in_keeps_an_old_artifact_with_no_verified_copy, test_sweep_artifacts_after_verified_copies_counts_the_held_files, the_snapshot_copy_uses_the_configured_cold_bucket
- [x] N3 (app) The 31 Oct wipe keeps every object under `instrument-snapshots/`.
  - Files: (crates/app/src/wipe_2026_10_31.rs, deleted 2026-10-29 after the 28 Oct run, commit 72f55c66b)
  - Tests: (test_classify_s3_key_keeps_every_instrument_snapshot, deleted 2026-10-29 with the tool)

### PR EX — owner 2026-10-28: the daily expiry check (app, storage, common, aws-lambdas, docs)

Approved by: owner 2026-10-28, "go ahead and add the daily expiry check" (a guarantee, with daily proof, that only the CURRENT expiry of each underlying is subscribed).

- [ ] EX1 (app, storage, common) Two layers. Subscribe time: after
  `select_contract_universe`, `enforce_subscribe_expiry_scope` recomputes each
  underlying's nearest real expiry from the day's contract file (a pass
  separate from the selector) and REFUSES any selected contract that is
  expired, on another expiry, or unknown, with one coded `EXPIRY-SCOPE-01`
  line. Daily at 15:50 IST: today's distinct option contracts in `candles_1m`
  judged against the same table, one `expiry_scope_audit` row (`clean` /
  `violation` / `partial`), at once when the process starts after 15:50 on a
  trading day. "Current" is judged per selector bucket (segment, index or
  stock option, underlying as the file spells it), so the check cannot refuse
  a contract the selector rightly picked. Also fixes the 2026-10-28 naming gap
  (HAVELLS-23Nov2026-1260-CE, security_id 114816, NULL `contract`): the attach
  replaced the boot's full candle-name table with the 25,000-capped ranking
  table, so a contract an earlier attach subscribed (the socket keeps it) lost
  its name; the attach now names every option row of the day's file
  (`option_labels_from`), as the boot does. Review fixes (same day): the
  daily read first waits (≤ 300 s) for the cross-check's three live-final
  barriers (`dhan_live_crossverify_boot::live_side_readiness`) and is never
  `clean` when they did not hold; a stored candle with no name makes the day
  `partial` (`unlabelled_rows`); an id the file lists on two different
  contracts is refused at subscribe time and unjudged daily; the name is
  compared on underlying, expiry, strike and leg; legs are counted exactly as
  the selector counts them (id 0 included); the subscribe refusal logs once
  per day per refused set; the audit row is written up to 3 times with the
  DDL re-run first; the attach's O(rows) passes run through `off_worker`.
  - Files: crates/app/src/expiry_scope_check.rs, crates/app/src/expiry_scope_check_tests.rs, crates/app/src/dhan_contract_universe.rs, crates/app/src/dhan_live_crossverify_boot.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/app/tests/ensure_ddl_boot_wiring_guard.rs, crates/storage/src/expiry_scope_audit_persistence.rs, crates/storage/src/lib.rs, crates/storage/src/partition_manager.rs, crates/storage/tests/ilp_overflow_bound_guard.rs, crates/common/src/error_code.rs, crates/common/tests/error_code_alarm_coverage_guard.rs, crates/common/tests/error_code_paging_decision_guard.rs, crates/aws-lambdas/src/operator_control_action_commands.rs, docs/error-runbooks/expiry-scope-error-codes.md, .claude/triage/error-rules.yaml, CLAUDE.md
  - Tests: test_parse_contract_label_keeps_a_hyphenated_underlying_whole, proptest_parse_contract_label_round_trips_the_renderer, test_build_expiry_index_skips_a_thin_stub_and_picks_the_real_chain, test_build_expiry_index_counts_the_expiry_day_itself_as_current, test_build_expiry_index_on_yesterdays_file_finds_nothing_current, test_check_selected_expiries_refuses_off_expiry_expired_unknown_and_the_stub, test_enforce_subscribe_expiry_scope_removes_and_counts, proptest_selector_output_always_passes_the_expiry_scope_check, test_classify_candle_contracts_the_havells_unlabelled_candle_counts_once, test_classify_expiry_scope_every_combination, test_main_wires_spawn_expiry_scope_check, test_the_daily_check_runs_on_its_own_task_never_the_frame_drain, test_the_attach_refuses_before_it_names_or_dials, test_expiry_scope_audit_ddl_statements_self_heal_every_column_and_never_drop, test_build_expiry_index_marks_a_duplicate_key_on_two_expiries_ambiguous, test_build_expiry_index_a_duplicate_key_on_one_contract_is_not_ambiguous, test_build_expiry_index_counts_an_id_zero_leg_like_the_selector, test_classify_candle_contracts_a_name_with_the_wrong_strike_or_leg_is_a_mismatch, test_refusal_fingerprint_dedupes_one_set_per_day, test_readiness_reason_is_clean_only_when_final_and_applied, test_classify_expiry_scope_a_live_side_not_final_is_partial_but_a_violation_still_wins, test_live_side_readiness_is_never_ready_without_a_drained_seal_writer, test_build_expiry_index_keeps_index_and_stock_buckets_apart, test_build_expiry_index_uses_the_underlying_as_the_file_spells_it, test_expiry_index_current_expiry_of_and_current_expiry_by_name, test_classify_candle_contracts_a_malformed_second_name_is_never_clean, test_classify_candle_contracts_hyphenated_underlying_name_only_is_judged, test_secs_until_expiry_scope_check, test_ymd_of_edges, test_option_labels_from_keys_exactly_as_a_candle_row_probes, test_regression_attach_publishes_every_option_name_uncapped

- [ ] S1 (core, app) Silence detector out of slots (live 2026-10-28,
  RISK-GAP-03 "ran out of slots"): the WAL replay folds frames from earlier
  sessions through `TickGapDetector::observe` before the contracts attach,
  every replayed contract takes a slot for the rest of the process, and the
  live contracts seeded afterwards are refused. A replay now calls
  `observe_tracked` (refreshes a key that already holds a slot, never
  assigns one, a skip is not a refusal); the live path is unchanged.
  - Files: crates/core/src/pipeline/tick_gap_detector.rs,
    crates/app/src/dhan_feed_stack.rs
  - Tests: test_observe_tracked_never_assigns_a_slot_and_never_counts_a_refusal,
    test_observe_tracked_replay_cannot_starve_the_live_universe_of_20519,
    a_wal_replay_cannot_use_up_the_silence_detector_slots_of_the_live_universe

- [x] OE1 (done: `order_update.rs`, `order_update_connection.rs`, branch order-evt-hollow) (core) ORDER-EVT-02 "hollow order updates", 18 live lines.
  Assumed cause (not Verified, no live excerpt available): the parser read the other casing
  of only seven fields, so a frame with camelCase order keys (`orderNo`, `status`,
  `securityId`) decoded with every identity field empty. The 2026-08-25 rows had `mkt_type`
  filled, which the old parser read only from PascalCase, so they do not fully fit this
  cause; ORDER-EVT-02 stays open until a captured excerpt confirms it. Fix: both casings of
  every field and of the wrapper; null field = absent except a fill field (null or empty
  quantity / filled quantity / price still fails loud); exact number/string coercion, ids
  integral only; every unreadable frame (hollow, or no order number with a foreign `Type`)
  stays ORDER-EVT-02 at ERROR with `msg_type` and is still broadcast and audited.
  - Files: crates/core/src/parser/order_update.rs, crates/core/src/parser/mod.rs,
    crates/core/src/websocket/order_update_connection.rs,
    docs/error-runbooks/order-update-events-error-codes.md, CLAUDE.md (O(1) row)
  - Tests: test_field_table_covers_every_order_update_field,
    test_parse_order_update_all_camel_case_frame_decodes_like_pascal,
    test_every_single_field_missing_or_null_still_parses,
    test_traded_update_with_blank_fill_fields_fails_loud,
    test_is_hollow_order_update_all_identity_permutations,
    test_unreadable_order_frame_type_verdicts, test_fix_for_field_coercions,
    prop_any_field_subset_and_casing_decodes,
    test_unreadable_order_frame_type_verdict_pages_and_still_broadcasts,
    test_live_hollow_shapes_parse_or_page

### PR DT-small — 2026-10-28 three drain waits over 1 ms (app, storage, trading)

- [ ] DT-small (app, storage, trading) Three confirmed causes of a frame waiting
  more than 1 ms on the frame drain (hostile review, 2026-10-28):
  DT-A1 the late-attach seed arm seeded a ~22,000-contract batch in one arm;
  now it queues the batch (a move) and the idle arm seeds
  `SEED_STEP_INSTRUMENTS` (512) per step, queue pre-sized to
  `SEED_QUEUE_BATCHES` (8), arm gated on room. DT-A2 the depth row buffer had
  no pre-size; now `DepthLayout::row_buffer_presize_bytes` (4 MiB level,
  9 MiB book) at construction and after a spare-less hand-off (the flush arm,
  or the depth size trigger inside the frame arm, so one allocation per
  spare-less hand-off can land on the frame path), like the tick writer's K6. DT-A6 the live fold was sized at the
  boot-time spot count, so the ~866th instrument moved the slot table and
  rehashed the index inside a frame; now `LiveIngest::with_fold_capacity(AGGREGATOR_MAX_SLOTS)`
  at the boot site (67.1 MiB of address space measured, committed as slots
  are written; the grow-once path already reserved the same ceiling).
  - Files: crates/app/src/dhan_feed_stack.rs, crates/storage/src/depth_persistence.rs,
    crates/trading/src/candles/multi_tf_aggregator.rs, CLAUDE.md
  - Tests: test_queue_seed_batch_and_step_seed_are_bounded_per_call,
    test_seed_queue_has_room_refuses_past_the_bound_and_never_grows,
    the_drain_has_a_seed_arm_that_does_not_outrank_frames (extended),
    test_with_fold_capacity_and_fold_slot_capacity_at_the_ceiling,
    test_slot_capacity_at_the_ceiling_never_reallocates_while_the_universe_fills,
    test_regression_depth_row_buffer_is_presized_and_never_regrows_on_append,
    test_row_buffer_presize_bytes_per_layout,
    test_step_idle_work_seeds_before_the_catch_up_sweep_then_finishes_it,
    a_depth_spare_of_another_protocol_is_never_installed (strengthened)

### Not in this plan (blocked by a rule, need their own dated quote)

- Shipping `tv_dhan_token_publish_total` / `tv_dhan_shared_token_adoption_total`
  to CloudWatch with an alarm: a new page needs a dated noise-lock row first.
- Skipping the 06:05 Lambda mint while a drain hold is fresh: changes the sole
  minter's schedule contract (token-minter §10), owner decision.
- Raising the root disk throughput back above 125 MiB/s: Quote 31 REJECTs it
  without a fresh dated quote (live: disk at the 125 MiB/s ceiling all
  session since the 21 Oct cut; Quote 32 step 2's "after one depth-off session
  peaks under 100 MiB/s" was not met: 21 Oct averaged 191 MiB/s).
- The REST stack re-trying after its lock patience (L3 5b, reverted).
- The `ticks` designated timestamp (M1 root cause): repeat quotes keep their old trade time, so most commits insert behind newer rows and QuestDB rewrites partitions (about 51 KB written per tick). Stamping repeat quotes with their receipt time, or moving them to a receipt-stamped table, changes the data-integrity rule for `ts` and needs the owner's dated quote first.

- Frame-silence fast path to `act`: the scope lock REJECTs defaulting to `act`
  without a dated measured row (websocket-connection-scope-lock.md 2026-10-10).
- Uncoded `error!` in `indicator/engine.rs` and `strategy/hot_reload.rs`: §28 frozen area.

### ITEM SC — brutex strategy consumer side (owner brief 2026-10-29, "WHAT TO DO NOW" A–D; no live change)

Approved by: Parthiban, 2026-10-29, the brief "MY DECISIONS (29 Oct 2026). They are final" (rule `brutex-strategy-host-2026-10-29.md`). Design, edge cases, failure modes, tests, rollback and observability for this item are in `docs/architecture/brutex-strategy-contract.md` §2–§5 and in the module headers; summary: pure logic in `crates/trading/src/brutex_host/` (no I/O), a read-only S3 lister/downloader in `crates/storage/src/strategy_recipe_fetch.rs` (List + Get only), the `StrategyEngine` trait with a test double, replay harness naming the first differing bar and field, pre-open swap decision, NDJSON audit; nothing is wired into the live trading path and no order is placed. Rollback: delete the modules (no caller on the live path).

- [x] SC1. Rules + docs (rule file, readonly-lock §1.3, contract doc with crate API and formats, build-access table + ls-remote check)
  - Files: .claude/rules/project/brutex-strategy-host-2026-10-29.md, .claude/rules/project/brutex-readonly-lock-2026-07-18.md, docs/architecture/brutex-strategy-contract.md
  - Tests: (docs only)
- [x] SC2. Recipe/manifest parse + sha256 verify + version/snapshot selection + gates + replay harness + swap decision + audit, pure, in crates/trading
  - Files: crates/trading/src/brutex_host/mod.rs, crates/trading/src/brutex_host/*.rs, crates/trading/Cargo.toml
  - Tests: test_manifest_hash_mismatch_refuses_and_keeps_previous, test_missing_file_refuses, test_version_gap_picks_newest_and_logs_gap, test_empty_candle_file_refuses, test_one_bar_file_replays, test_signal_mismatch_on_last_bar_names_bar_and_field, test_two_faults_at_once_report_the_first_check_that_fails, test_may_swap_and_apply_only_before_the_open_on_a_trading_day
- [x] SC3. Read-only S3 fetch (List + Get) into a local staging folder, in crates/storage
  - Files: crates/storage/src/strategy_recipe_fetch.rs, crates/storage/src/lib.rs
  - Tests: test_fetch_strategy_never_calls_a_write_api (source scan), test_plan_download_picks_newest_version_two_newest_params_and_newest_universe
- [x] SC4. Dry run on a fixture recipe (a `strategy-dry-run --dir` app subcommand that loads, verifies, gates and replays a fixture with the test double, printing the audit lines)
  - Files: crates/app/src/strategy_dry_run.rs, crates/app/src/main.rs, crates/trading/src/brutex_host/fixture.rs
  - Tests: test_dry_run_on_fixture_accepts, test_dry_run_on_tampered_fixture_refuses

### ITEM SP — the sealed-candle path under 1 µs per seal (owner directive 2026-10-30; storage)

Approved by: Parthiban, 2026-10-30 ("every in-memory step must take under 1 microsecond at p99, proven by benches that fail the build if it slips, with identical output; use only max"). Scope: what happens to a sealed candle after the fold produces it, in `crates/storage` only (the fold, `crates/trading/src/candles`, belongs to another session). Design: measure each per-seal step with a new Criterion bench, fix what is over 1,000 ns or allocates, keep the ILP bytes identical. Edge cases: a ring that already holds seals is never prefaulted; a refused row still rewinds to its marker; the `contract` name is looked up once per row by the caller. Failure modes: an invalid constant column name would reach QuestDB unchecked (test runs the library check over every name and a source scan refuses a literal); the reused drain buffer is cleared on every return path. Rollback: revert the commit (no schema, config or wire change). Observability: the bench prints p50/p99/p99.9/max per step; no new metric.

- [x] SP1. `seal_path` Criterion bench for the hand-off, take + ring push, row build, ILP append (with and without contract names), ring pop + append, ledger probe hit and miss, with its own p99 gate against the new `seal_path` budget (1,000 ns) for the sub-µs steps and `seal_ilp` (2,500 ns, measured ceiling, NOT the 1 µs target) for the three ILP-append steps
  - Files: crates/storage/benches/seal_path.rs, crates/storage/Cargo.toml, quality/benchmark-budgets.toml, CLAUDE.md
  - Tests: every_latency_budget_reaches_at_least_one_benchmark, claude_md_states_the_real_budget_key_count
- [x] SP2. The live drain reuses one rescue buffer (`drain_once_reusing`, runner `drain_scratch`)
  - Files: crates/storage/src/seal_writer_task.rs, crates/storage/src/seal_writer_runner.rs
  - Tests: test_drain_once_reusing_keeps_its_scratch_capacity_across_cycles
- [x] SP3. The production ring is prefaulted at construction (`prefault_ring`)
  - Files: crates/storage/src/seal_absorption.rs
  - Tests: test_prefault_ring_writes_every_slot_and_leaves_the_ring_empty, test_production_pipeline_is_prefaulted_at_construction
- [x] SP4. Constant column names skip the per-call questdb-rs check; the delay text skips `fmt`; bytes identical
  - Files: crates/storage/src/shadow_candle_writer.rs, crates/storage/src/delay_render.rs
  - Tests: proptest_write_row_is_byte_identical_to_the_validated_form, test_every_candle_ilp_column_name_passes_the_questdb_check, test_write_row_passes_only_named_constants_to_the_unchecked_names, proptest_render_delay_into_matches_the_fmt_version_byte_for_byte, test_render_delay_into_matches_the_fmt_version_at_every_edge
- [x] SP5. DHAT: the whole per-seal path allocates nothing
  - Files: crates/storage/tests/dhat_seal_per_seal_path.rs
  - Tests: dhat_the_per_seal_path_after_the_fold_never_allocates
- [x] SP6. Review fixes: the candle ILP buffer is presized and written once at construction (no regrowth on the append); a p99 tail miss defers to a step after the Criterion baseline save
  - Files: crates/storage/src/shadow_candle_writer.rs, crates/storage/benches/seal_path.rs, crates/app/benches/feed_drain.rs, .github/workflows/bench.yml, crates/common/tests/shell_budget_guard.rs, crates/storage/tests/bench_tail_gate_defer_guard.rs
  - Tests: test_candle_ilp_buffer_presize_covers_a_full_drain_of_full_rows, test_presized_buffer_keeps_capacity_through_clear_and_writes_no_rows, both_benches_print_the_marker_and_exit_unless_deferred, bench_yml_defers_and_fails_after_the_baseline_save, workflow_run_lines_never_grow
- [x] SP7. The candle row through #2163's own ILP encoder, behind `[tick_storage] own_ilp_encoder` (default off): `IlpLine` gains `column_str` (the library's quoted escape), the candle writer's buffer and sender become `TickRowBuffer` / `TickIlpSender`, byte-identical to the validated library form, zero allocation; own-encoder steps held to `seal_path` = 1,000 ns at p99 (measured p99 780 to 825 ns)
  - Files: crates/storage/src/tick_ilp.rs, crates/storage/src/tick_ilp_tests.rs, crates/storage/src/shadow_candle_writer.rs, crates/storage/src/seal_writer_runner.rs, crates/app/src/main.rs, crates/storage/benches/seal_path.rs, crates/storage/tests/dhat_seal_per_seal_path.rs
  - Tests: proptest_own_encoder_candle_rows_are_byte_identical_to_the_validated_form, test_own_encoder_writer_appends_the_same_bytes_as_the_library_writer, test_flush_error_is_connection_matches_each_encoder, test_own_encoder_test_writer_flush_fails_and_keeps_rows, test_column_str_matches_the_library, test_column_str_before_table_is_refused_like_the_library, dhat_the_per_seal_path_after_the_fold_never_allocates

### Muhurat 2026-11-08 (Sunday) — the weekday-only protections

Approved by: Parthiban, 2026-10-30 ~12:00 IST, "go ahead and fix the muhurat risk dude" (recorded as `daily-universe-scope-expansion-2026-05-27.md` Quote 49 and noise lock §2.15). Design: `[[trading.muhurat_trading_dates]]` in `config/base.toml` gains the session times and becomes the one source (calendar `is_session_day`, `muhurat_session_secs`); terraform's copy is pinned equal; the workflow guards read the date from the same file; the liveness gate gets dated open/close rules. Edge cases: an ordinary Sunday and a weekday holiday stay closed; times empty = box not started (unchanged); an evening session must fit 18:00–19:30 (build check); the did-it-run alarms are never armed on a weekend. Failure modes: config file missing in a guard job reads "not Muhurat" (fail-open), so every guard job checks the file out and a ratchet pins it; reported times wrong: the box still runs from start − 60 min and the arming offset absorbs a later first tick. Rollback: revert the PR (no data or schema change). Observability: no new page; the gate Lambda logs its armed list.

- [x] MU1. Calendar session times + session-day check + capture-window build check (crates/common)
  - Files: config/base.toml, crates/common/src/config.rs, crates/common/src/trading_calendar.rs, crates/common/src/muhurat.rs, crates/common/tests/muhurat_session_calendar.rs, crates/common/tests/config_round_trip.rs, crates/common/benches/calendar.rs, crates/app/tests/pipeline_smoke.rs
  - Tests: test_is_session_day_sunday_muhurat_with_its_times, test_muhurat_session_secs_ordinary_sunday_and_weekday_holiday_stay_closed, test_muhurat_on_a_weekday_holiday_is_a_session_day, test_malformed_session_times_refuse_the_calendar, every_configured_muhurat_session_is_inside_the_capture_windows, session_capture_windows_accept_afternoon_and_evening_sessions
- [x] MU2. Terraform times pinned to the config; liveness gate armed on the Muhurat date without the weekday did-it-run alarms (crates/aws-lambdas, terraform)
  - Files: deploy/aws/terraform/variables.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, crates/aws-lambdas/src/market_hours_gate.rs, crates/aws-lambdas/tests/muhurat_window_wiring_guard.rs
  - Tests: terraform_session_times_equal_the_app_calendars_times, each_muhurat_date_arms_and_disarms_the_liveness_gate, test_alarms_to_open_muhurat_event_leaves_the_skipped_alarms_disarmed, test_alarms_to_open_never_widens_the_list
- [x] MU3. Every workflow market-hours guard refuses the whole Muhurat date (workflows)
  - Files: .github/workflows/deploy-aws.yml, .github/workflows/terraform-apply.yml, .github/workflows/aws-control.yml, .github/workflows/downsize-instance.yml, .github/workflows/emergency-fs-recover.yml, .github/workflows/wipe-log-streams.yml, crates/common/tests/muhurat_workflow_freeze_guard.rs, crates/common/tests/aws_infra_wiring.rs
  - Tests: every_weekday_market_check_also_closes_on_a_muhurat_date, every_guard_job_has_the_calendar_file

### ITEM DASH — F&O stocks price movers on the dashboard (api; 2026-10-30)

Requested: owner brief relayed to this session on 2026-10-30 ("yes add the fno stocks page to dashboard dude"). The brief reached the session as an automated notification, so the owner's approval of the item and of the dashboard script growth is to be confirmed on the PR. Design: one read-only endpoint `GET /api/fno-stocks` (bearer-gated, on the shared public rate limiter, 2 s single-slot cache) runs ONE bounded QuestDB query in session only (`LATEST ON ts PARTITION BY security_id, segment` over the last 15 min of `ticks`, `segment = 'NSE_EQ'`, `feed = 'dhan'`, ids from today's F&O underlying list); the list file is read once per IST date. Change is against `ticks.close` (the previous session's close on Quote/Full packets); value is an estimate (volume x avg price). The dashboard gets one section in its existing script (no new tag, no new page). Edge cases: market closed, list missing, empty result, NULL/zero/NaN previous close, an id in two segments, QuestDB timeout or error, stale newest row. Failure modes: every failure is a named status, never an empty "no movers"; only `live` is shown as live. Rollback: revert the PR (no config, no table, no alarm). Observability: `tv_api_cache_hits_total{endpoint="fno_stocks"}`; the status field.

- [x] DASH1. Endpoint, cache, route, page section, writer/reader pin
  - Files: crates/api/src/handlers/fno_stocks.rs, crates/api/src/handlers/mod.rs, crates/api/src/state.rs, crates/api/src/lib.rs, crates/api/src/handlers/dashboard_page.rs, crates/app/src/dhan_universe.rs
  - Tests: test_build_fno_stocks_sql_is_bounded_keyed_and_constant, test_empty_result_is_no_data_not_live, test_one_row_is_live_with_change_and_value, test_zero_and_nan_prev_close_give_no_change, test_rows_sort_by_change_pct_with_no_change_last, test_same_security_id_in_two_segments_keeps_only_nse_eq, test_questdb_timeout_is_unreachable, test_fno_stocks_route_requires_the_bearer_token, test_get_fno_stocks_cache_hit_skips_the_query, test_dashboard_page_has_the_fno_stocks_movers_section, test_fno_list_path_tag_and_shape_match_the_dashboard_reader
- [x] DASH2. Raise the dashboard's JS budgets in `browser_surface_and_toolchain_guard.rs` (235 → 301 lines, 8,781 → 12,194 bytes) after the owner's dated note is recorded in `rust-only-forever-lock-2026-07-19.md` (that guard requires the note first). Note recorded as §0.15 by the coordinator session (commit b06c382); numbers raised to the measured sizes.
  - Files: crates/common/tests/browser_surface_and_toolchain_guard.rs, docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md, .claude/rules/project/rust-only-forever-lock-2026-07-19.md
  - Tests: javascript_volume_inside_script_tags_only_shrinks, javascript_bytes_inside_script_tags_stay_under_the_ceiling

### ITEM O1S — per-event O(1) / allocation sweep outside the owned files (storage, core, api; 2026-10-30)

Requested: owner, 2026-10-30, relayed by the coordinator session ("O(1) at p99 everywhere", "use only max"). Scope: find every per-event path (tick, frame, seal, order, request, alert) that is not O(1) or allocates and is not in CLAUDE.md's non-O(1) table; fix it in files no other session owns with byte-identical output, else add an honest row. No new page, alarm, EMF name, config switch or cost change. Design, failure modes, rollback and observability for these edits follow the plan sections below: each fix is local, keeps every outcome and written byte unchanged, and reverts with one commit.

Reconciliation 2026-10-31: the current base already keeps the rescue scratch buffer through `drain_once_reusing` / `drain_scratch` and prefaults the production ring. Preserve those canonical paths and their existing tests, add this branch's episode accounting and comparison assertions, and retain the original branch history. Validation on 2026-10-31: 319 distinct focused storage/core/API/common cases passed, including the seal and Telegram DHAT ratchets; each of five isolated negative controls failed its intended assertion and passed after exact restoration. The restored absorption/coalescer modules then passed 70 repeated cases, and workspace all-targets Clippy passed with warnings denied. These repeated cases are not added to the distinct total. The live-flush rearm requires a nonzero pending-row count before a successful synchronous HTTP send: refused-only empty flushes and failed nonempty flushes leave the episode open. HTTP acceptance does not prove WAL application, power-loss durability or recovery of the failed disk tier. No new timing measurement was performed; committed-head full tests, normal publication gates and CI remain pending.

- [x] O1S1. Seal escalation fault path: constant record length for the floor, per-episode spill/DLQ failure lines (`FailureEpisode`), literal blind-probe label; seal writer keeps its rescue buffer across cycles (`drain_once_reusing`)
  - Files: crates/storage/src/seal_absorption.rs, crates/storage/src/seal_writer_task.rs, crates/storage/src/seal_writer_runner.rs
  - Tests: test_failure_episode_opens_once_and_closes_with_its_count, test_repeated_spill_failures_keep_every_outcome_and_latch_one_episode, test_floor_payload_constant_equals_the_serialised_record_length, test_blind_probe_tier_label_literal_matches_the_const, test_drain_once_reusing_matches_drain_once_and_reuses_its_buffer, test_note_clean_flush_ends_an_episode_so_the_next_failure_logs_again, test_live_clean_flush_rearms_failure_episodes_through_the_production_drain, test_refused_only_live_flush_keeps_failure_episodes_open, test_failed_nonempty_live_flush_keeps_failure_episodes_open, test_spill_recovery_ends_the_dlq_episode_so_a_later_double_failure_logs
- [x] O1S2. Telegram coalescer renders a body only when the bucket keeps it, one entry match instead of clone-per-fold (crates/core)
  - Files: crates/core/src/notification/coalescer.rs
  - Tests: test_payload_rendered_only_while_the_bucket_keeps_samples, test_sample_cap_at_10
- [x] O1S3. `require_bearer_auth` borrows the request path; `classify_auth_response` comment corrected (crates/api, crates/core)
  - Files: crates/api/src/middleware.rs, crates/core/src/websocket/order_update_connection.rs
  - Tests: test_classify_auth_response_dhan_error_code, test_classify_auth_response_error_code_without_message
- [x] O1S4. CLAUDE.md O(1) table: nine new rows plus the `active_order_count` correction; owned-file findings recorded as proposals
  - Files: CLAUDE.md
  - Tests: every_rs_file_named_in_claude_md_table_exists

### ITEM MOV1 — Stocks movers show the ended session's stored prices after the close (2026-11-03)

Requested: owner, 2026-11-03 ("in top movers we should always see the data ... which ever data is completely available in live AWS server should be at least allowed to be displayed"), design approved on the decision card "Approve, build it". This PR is step 1 (server on). Step 2 (a daily S3 file so the page works while the server is off, plus a date/time picker) is a later PR under the same approval.

- [x] MOV1a. After the close, `/api/fno-stocks` and `/api/stock-movers` read the session that just ended (`[09:15, min(now, 15:40)]` IST) instead of returning an empty `closed`; status stays `closed`, `market_open` false; before 09:15 or on a day with no rows it stays empty `closed`; query failures keep their own status
  - Files: crates/api/src/handlers/fno_stocks.rs, crates/api/src/handlers/stock_movers.rs, crates/common/src/operator_stock_movers_wire.rs, crates/common/src/operator_movers_wire.rs, crates/aws-lambdas/src/operator_control_console.html, crates/aws-lambdas/src/operator_control.rs
  - Tests: test_closed_market_shows_the_ended_sessions_stored_prices, test_closed_market_with_no_rows_reads_closed_not_no_data, test_closed_market_failure_stays_a_failure_and_never_reads_open, test_closed_session_window_bounds, test_closed_sql_carries_the_upper_bound_and_live_sql_does_not, closed_stale_partial_empty_and_query_failure_remain_distinct, console_html_content_pin
- [x] MOV1b. Date and IST time selection on Stocks → Price Movers (owner 2026-11-03: "existing data … with available dates and times for selection"): the movers filter gains optional `as_of` (`YYYY-MM-DDTHH:MM`, shape-checked on the wire, calendar-parsed by the API, 400 when not a real minute); a chosen minute is always a stored-session read (`closed`, `market_open` false, window `[that day 09:15, min(at, that day 15:40)]`, that day's lists); the page adds a date input (min 2026-11-02) and a time input (empty = 15:30); JS bytes 34,530 → 34,714 under rust-only lock §0.18
  - Files: crates/common/src/operator_stock_movers_wire.rs, crates/api/src/handlers/fno_stocks.rs, crates/api/src/handlers/stock_movers.rs, crates/aws-lambdas/src/operator_control_console.html, crates/aws-lambdas/src/operator_control.rs, crates/common/tests/browser_surface_and_toolchain_guard.rs, docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md, .claude/rules/project/rust-only-forever-lock-2026-07-19.md
  - Tests: test_valid_as_of_shape_accepts_only_a_full_ist_minute, chosen_minute_reads_the_stored_session_even_while_the_market_is_open, console_html_content_pin, javascript_bytes_inside_script_tags_stay_under_the_ceiling

### ITEM ILP1 — cold-path ILP writers pin the protocol so a slow QuestDB at boot cannot leave them dead all day (2026-11-03)

Found live 2026-11-03: `ws_event_audit` (19:19 IST) and `feed_gap_audit` (17:21 IST) logged "QuestDB unreachable — buffering locally" at construction. Their conf had no `protocol_version`, so `Sender::from_conf` read QuestDB's `/settings` while building; it timed out, the writer kept `sender: None`, and `flush` bails for the rest of the process. Design: pin `protocol_version=1;` in the seven storage confs that lack it (the tick, depth, order and latency writers already pin it), so building makes no network call and a slow QuestDB is a retried flush error with rows retained (bounded by `ilp_overflow`). Edge cases: the disconnected buffer was already V1, so a pinned V1 sender never mismatches it; these tables hold no array columns, so V1 text doubles carry the same values. Failure modes: QuestDB down keeps flush failing and retrying, never a silent drop beyond the existing overflow bound. Rollback: revert; behaviour returns to probe-at-build. Observability: unchanged (existing flush-error logs and `ilp_overflow` counters).

- [x] ILP1. Pin `protocol_version=1;` in the seven storage ILP confs without it, plus a source-scan guard and a no-network build test
  - Files: crates/storage/src/ws_event_audit_persistence.rs, crates/storage/src/feed_gap_audit_persistence.rs, crates/storage/src/feed_episode_audit_persistence.rs, crates/storage/src/feed_scoreboard_persistence.rs, crates/storage/src/ws_connection_daily_persistence.rs, crates/storage/src/tick_conservation_audit_persistence.rs, crates/storage/src/table_storage_probe.rs, crates/storage/tests/ilp_conf_pins_protocol_guard.rs
  - Tests: every_ilp_http_conf_in_storage_pins_the_protocol_version, a_pinned_conf_builds_a_sender_without_reaching_questdb

## Design

PR A: a tiny clock (`UnbackedDepthClock`, Option<tokio Instant>) owned by the
drain. Each frame arm pass reads `has_unbacked_pending()` (one bool). While false
the clock is cleared; while true it starts once, and when `FLUSH_INTERVAL` has
passed the frame arm calls `flush_depth` and restarts it. The timer arm keeps its
own 500 ms check, so whichever arm runs first hands the rows on. No new task, no
lock, no allocation.

PR J: each fix keeps the existing tiers and only closes a gap: the rescue deferral asks the WAL fate table (one atomic load) before trusting replay; the shutdown drain is a bounded try_recv loop; ensures reuse spawn_ensure_until_keyed; core limits reuse websocket_config; guards are source scans.

PR K: each fix keeps the existing tiers: replay classification and streak stop are pure functions; the WAL sync failure reuses the existing lost mark; the silence spacing reuses the existing per-kind CAS; verdict counters are pre-resolved handles.

PR M: every item starts from a live measurement and ends with a before/after number; fixes reuse existing tiers.

PR DT-small: each fix moves work off a frame, never adds any: the seed batch rides the existing idle arm (`consume_budget`, last in the biased select) as a bounded step before the catch-up seal step; the depth buffer reuses the K6 reserve-at-hand-off shape (for depth the hand-off can run inside the frame arm, after the frame that tripped the size trigger: one allocation per spare-less hand-off there, never per row); the fold is built at the ceiling the grow-once path already reserved, with the same builder shape as `with_detector_capacity`.

PR L: each fix reuses an existing mechanism: the owed close code is already stored in `pending_close`; the seal floor reuses `classify_spill_floor`; the Muhurat gate reuses the writers' Muhurat flag; workflow guards reuse the deploy-aws IST check; budget distrust widens an existing day-1 rule.

PR N (Quote 44): the rider asks for a snapshot pass on its own task after each successful build; the pass lists data/instrument-cache, uploads every dated .json with no matching marker through raw_frame_upload::upload_file (gzip, conditional put, HeadObject verify, marker), and the 7-day sweep inside build_once deletes only marked files. The lifecycle writer and its storage helpers are deleted.

PR EX (design): two layers. The attach refuses an off-expiry contract before it is named, mapped or dialled (fail-closed: a wrong-expiry contract looks healthy and mixes two chains); the 15:50 IST task reads today's distinct option contracts from `candles_1m` (bounded, 100,001 rows, 60 s timeout, 3 reads 120 s apart), judges each contract once on its composite key, and writes one `expiry_scope_audit` row (DEDUP `ts, trading_date_ist, feed`). "Current expiry" is recomputed by its own pass with the selector's rule (smallest expiry >= today with at least 4 legs), so the two can disagree only if one of them is wrong. The bucket mirrors the selector's exactly (index and stock options apart, the underlying untrimmed), because the subscribe-time layer refuses: a looser key would merge two chains and drop a correct contract. A stored name the file does not list carries no class, so it is judged by trimmed name only when every bucket of that name agrees. A process that starts after 15:50 IST on a trading day runs the check at once (one row per run, `ts` in the key). Candle names at the attach come from every option row of the day's file, uncapped, as at boot.

S1: the detector stays sized once at the authorized ceiling
(`AGGREGATOR_MAX_SLOTS`, 25,000, `with_detector_capacity`), which covers the
whole main-feed universe (5 sockets × 5,000). Only the slot ASSIGNMENT moves:
`ingest_tick_at` reads the existing `replaying_wal` flag and calls
`observe_tracked` during a replay, `observe` otherwise. Slots are therefore
taken only by the subscribe set (boot seed + `seed_rx`) and live ticks.

OE1 (ORDER-EVT-02): the order-update parse reads both casings of every `OrderUpdate` field from one table (`ORDER_UPDATE_FIELDS`) pinned to the struct's serialized keys, and of the `Data` / `Type` wrapper; a null field reads as absent, except a fill field (`FILL_CRITICAL_FIELDS`: quantity, filled quantity, remaining quantity, prices), where a null or empty value in either casing still fails the parse loudly so a TRADED update never decodes with a defaulted 0; a value in the wrong JSON type is converted only when it converts exactly, and an order or security id only from an integral number. `unreadable_order_frame_type` decides the ORDER-EVT-02 line: a hollow frame (no order number, status or security id) or a frame with no order number whose `Type` names something other than `order_alert`; it never filters, every frame is still broadcast and audited. All on the order-update read task, cold; one table walk per frame, one extra parse only for a frame with no order number.

## Edge Cases

- No depth sink: `is_some_and` gives false; nothing changes.
- Flush fails or the offload queue is full: the writer keeps the flag; the clock
  restarts, so the next attempt is 500 ms later, never once per frame.
- Rows marked unbacked between flushes keep the oldest start.
- tokio Instant is monotonic; `saturating_duration_since` guards anyway.
- Ring idle: the timer arm still flushes at 500 ms as before.

- PR J: a WAL that has not flushed through a batch (disk full) sends the batch to the inline spill instead of trusting replay; a shutdown ring backlog larger than the budget is counted, never silently dropped.

- PR K: a QuestDB that refuses every line (schema drift) stops the round after 3 quarantines instead of emptying the backlog; a 401/403 is retried, never quarantined; all main-feed sockets silent at once are staggered, one redial per spacing.

- PR M: a session that saturates the disk; a backlog larger than one boot's replay budget; a check that reads rows still in spill.

- PR DT-small: more than 8 seed batches waiting (the arm stops receiving and the 8-deep channel holds the rest; a call anyway seeds inline, never drops); a drain whose catch-up sweep never finishes (seed steps run first, so seeding still ends in at most ceil(n/512) idle steps); a book batch past 8 MiB plus one frame (grows by doubling as before); more than 25,000 instruments (refused and counted as before).

- PR L: a disconnect code stacked behind data at a failed write; a QuestDB that accepts TCP and never answers at exit; a Muhurat session in the evening; Monday 2 November with October's total still showing; a deploy failing at 09:10.

PR N: a file replaced mid-copy (marker no longer matches, copied again next pass); a same-day rebuild with different bytes (sidecar key, warning); an orphan .json.tmp (kept, counted); no bucket resolves (no_bucket counted, nothing swept); a request during a running pass (skipped_busy counted, rerun queued).

PR EX: a hyphenated underlying (`BAJAJ-AUTO`, parsed from the right); a decimal strike; a capped name (malformed, judged from the file); a thin earlier expiry (skipped, as the selector does); the expiry day itself (current); yesterday's file (every contract expired, never clean); one contract stored with and without a name (counted once, unlabelled); two names for one contract (label_mismatch); one good and one malformed name (malformed, never clean); a BSE contract (judged on BSE_FNO); a composite key listed twice on two different contracts (refused at subscribe time, unresolved daily, never clean); the same row listed twice (one contract, judged); a name on the wrong strike or leg (label_mismatch); a 4-leg expiry with an id-0 leg (real, as for the selector); the same refused set on every attach retry (one error line a day); an index and a stock option sharing an underlying name (separate buckets); an underlying spelled with a trailing space (its own bucket, as in the selector); a restart after 15:50 (checks at once); a restart mid-day (the subscribe count reads -1 only if this process never attached that day).

- S1: a replayed tick for a seeded spot still refreshes it (no boot page
  storm, the 2026-08-28 reason for the replay clock); a replayed tick for a
  contract not yet seeded is skipped, so that contract starts NeverTicked at
  its seed, the same as on a boot with no backlog; a live universe above the
  ceiling is still refused and counted (fail-closed kept).

OE1: a frame with both casings keeps the accepted key (as since 2026-07-14); `Type` null, a number or an object is dropped so it never costs the event; a null `TradedQty` / `tradedQty` / `AvgTradedPrice` or an empty one fails loud, a missing one is still a partial update; a value that does not convert exactly (`1.5` for a quantity, `12.5` for an order number, `"ten"`, a bool) still fails loud as `typed_parse_failed`; an oversized or unparseable frame logs an empty `msg_type` and still pages if hollow.

## Failure Modes

- Extra flushes under load: at most one per 500 ms from the frame arm.
- A crash still loses unbacked rows inside the 500 ms window (unchanged, stated).

- PR J: an inline spill that also fails is counted lost (unchanged accounting, now honest).

- PR K: a retried 4xx that never heals keeps the file waiting (counted, not quarantined); a staggered silence redial recovers the pool more slowly (at most 5 × spacing).

- PR M: a fix that moves writes rather than removing them shows up in the next session's EBS bytes.

- PR DT-small: an instrument attached while its batch waits is not yet in the silence detector, so its silence is judged up to ceil(n/512) idle steps later (milliseconds), and the 30 s silence scan, an arm above the idle arm, can run on a part-seeded batch and under-count never-ticked instruments for at most one scan per attach batch; the pre-sized buffers and fold take address space at boot whether or not the session fills them.

- PR L: a lane retry that never gets the lock keeps retrying on a capped backoff (logged per episode); a budget reading distrusted on day 2 pages instead of stopping.

PR N: S3 down or hung (counted failure, pass bounded at 30 min, stops after 3 time-outs in a row, files kept); marker write fails (verified copy held in memory, shared marker counter); a folder entry unreadable (skipped, counted); the console wipes still remove instrument-cache (documented limit, not changed).

PR EX: the contract file is missing (partial, artifact_missing); the read fails three times (partial, read_failed); more rows than the cap (partial, row_cap_hit unless a violation is already proven); the live side not sealed, drained or applied by 15:55 (partial with the barrier's reason; a violation found is still reported); a stored candle with no name (partial, unlabelled_rows); the audit write fails (retried 3 times 60 s apart with the DDL re-run first, then a coded STORAGE-GAP-03 line, the verdict stays in the log); the selector picks a stub or a later expiry (refused, coded line, the rest dials).

- S1: a contract that attaches after a mid-session restart reads NeverTicked
  until its first live tick (before, the replay pre-registered it); the
  K3 verdict counters no longer count replayed ticks of untracked keys.

- OE1: if the camelCase cause is wrong, ORDER-EVT-02 keeps firing at the same rate, now with `msg_type` and the excerpt to write the real fix against. An undocumented frame with no order number is logged and broadcast, never dropped (the order book ignores an unknown order at debug). No frame is kept from the order book or the audit tables.

## Test Plan

Unit and property tests on the clock; the existing source assertion extended to
pin the frame-arm call; the new DHAT test; `cargo test -p tickvault-app --lib`,
`dhat_live_ingest_seam`, source size and scan budget guards, clippy, fmt.

PR J: per-crate `cargo test -p` for storage, app, core, api, common; new self-test rows for every guard; clippy and fmt.

PR K: per-crate `cargo test -p` for storage, app, core; one regression test per item; clippy and fmt.

PR M: per-crate tests plus the next live session's EBS throughput, spill count and cross-check verdict read back read-only.

PR DT-small: `cargo test -p tickvault-trading`, `-p tickvault-storage`, `-p tickvault-app --lib` plus the DHAT drain tests (`dhat_live_ingest_seam`, `dhat_depth_drain`); one test per fix; the next live session read back: `ring_dwell` (max and buckets, 09:00 to 09:15) for A1, which runs in its own select arms outside the `drain_main_frame` timing, and `drain_main_frame` for A2 and A6.

PR L: per-crate `cargo test -p` for core, storage, trading, app, aws-lambdas; restart differential; workflow window pins; clippy and fmt.

PR N: cargo test -p tickvault-app --lib -- instrument_snapshot dhan_universe; the ensure_ddl_boot_wiring_guard and universe_boot_race_guard test binaries; cargo test -p tickvault-storage --lib instrument_lifecycle; cargo test -p tickvault-app --lib wipe_2026_10_31.

PR EX: unit tests for the name parser, the nearest-expiry table, the subscribe check and its mutation, the daily classifier and every verdict combination; proptests (renderer round trip, parser never panics, the real selector's output always passes the check); DDL self-heal and DEDUP-with-feed tests; source scans that main.rs spawns the task, the frame drain never calls it, and the attach refuses before it names. Commands: cargo test -p tickvault-app --lib, cargo test -p tickvault-storage, the changed guard binaries, crates/common tests.

S1: `cargo test -p tickvault-core --lib tick_gap_detector`, the app regression
(replay 25,000 prior-day keys, then seed 20,519, none refused), the existing
detector capacity tests and `dhat_live_ingest_seam`, clippy, fmt.

OE1: `cargo test -p tickvault-core --lib order_update` (parser + connection), the three order-update guard tests in `crates/core/tests/`; every field missing and null one at a time (fill fields must fail), all 8 identity-field permutations, blank fill fields on a TRADED frame, a proptest over random subsets and casings, the verdict over hollow, foreign-typed and readable frames.

## Rollback

Revert the PR: the frame arm loses the extra check; the timer arm's 500 ms flush
still runs whenever it is polled (the pre-PR behaviour).

PR I: revert the PR (no runtime switch, per the 2026-10-20 REJECT row); the
planner then swaps every minute as before.

PR J: revert the merge commit of each sub-branch; no config flag or schema change.

PR K: revert the merge commit of each sub-branch; no config flag or schema change.

PR M: revert the merge commit of each sub-branch.

PR L: revert the merge commit of each sub-branch; no schema change.

PR N: revert the PR. Reverting does NOT restart the lifecycle write lawfully: Quote 44 REJECTs a restart without a fresh dated quote. The snapshot objects already copied stay in S3 (kept forever).

PR EX: revert the commit. The table stays (nothing drops it); with the code gone nothing writes it. The candle names at the attach revert to the capped ranking table, which restores the 2026-10-28 naming gap only.

S1: revert the commit; no config flag or schema change.

OE1: revert the change; no schema, table, config or alarm change.

PR DT-small: revert the commit; no config flag, schema or metric change.

## Observability

No new metric: depth flushes and drops are already counted by the writer and
`depth_dropped`. The fix shortens a window and adds no failure path.

PR W (2026-10-20, PR W review round 4): local `/metrics` only, no CloudWatch
filter, no EMF name, no alarm: `tv_wal_unapplied_pinned_segments` and
`tv_wal_unapplied_pinned_bytes` (after every trim, and since pass 2 after
the boot fold and the backlog guard), `tv_wal_replay_disk_floor_stops_total`
(existing, read with them),
`tv_wal_unapplied_trim_refused_total{reason}`, and, registered at 0 at boot,
`tv_wal_suspect_episodes_total` and `tv_wal_suspect_seconds_total`.
PR I: one new label value, `outcome="min_hold"`, on the existing local
`tv_depth200_ranked_swaps_total` (pre-registered at 0; no EMF name, filter or
alarm); `held_back` and `min_hold` in the existing per-minute steering `info!`.

PR J: `tv_ticks_lost_total{source="rescue_abandoned"}` and `{source="shutdown_ring_unbacked"}`, registered at 0 at boot, local `/metrics` only (they ride the existing loss-group filter only after a dated noise-lock row).

PR K: tick-gap verdict counters and WAL staging-failure counters, local `/metrics` only, no EMF name, filter or alarm.

PR M: no new alarm or page; local `/metrics` only for any new counter.

PR DT-small: no new metric, alarm or page; the seed line keeps its wording (now once per finished batch), one boot `info!` names the fold's slot capacity, and the existing `tv_hot_path_stage_max_ns` shows the effect: `stage="ring_dwell"` for A1 (seeding runs in its own arms, outside the `drain_main_frame` timing), `stage="drain_main_frame"` for A2 and A6.

PR L: new reason labels and refusal counters only, local `/metrics` only; no new alarm or page (the liveness alarm's schedule moves, the page is unchanged).

PR N: tv_instrument_snapshot_upload_total{outcome=uploaded|reused|failed|no_bucket|skipped_busy|entry_unreadable} and tv_instrument_snapshot_sweep_held_total, registered at 0 at rider start, local /metrics only; failures log a coded STORAGE-GAP-04 error. No new alarm or page (needs a dated noise-lock row first).

PR EX: `tv_expiry_scope_subscribe_refused_total`, `tv_expiry_scope_subscribe_checked`, `tv_expiry_scope_daily_runs_total{verdict}`, `tv_expiry_scope_{off_expiry,expired,unlabelled}_contracts` (local `/metrics` only, registered at 0 when the task spawns); `expiry_scope_audit` rows; coded `EXPIRY-SCOPE-01` (log only, no filter, alarm, EMF name or page: Dhan noise lock §3): `error!` on a refusal (once per day per refused set) or a daily violation, `warn!` with `verdict = "partial"` and its reason when the day cannot be vouched for; `refused_off_expiry` on the attach's `contract universe resolved` line.

S1: no new metric, alarm or page. The existing `tv_dhan_feed_silence_detector_refused`
gauge and the edge-latched RISK-GAP-03 "ran out of slots" line read 0 / stay
quiet once the fix is live; `tracked` in the scan lines should equal the
subscribed count.

OE1: no new counter or alarm; new `kind` labels on `tv_order_update_decode_quirks_total` (local only); ORDER-EVT-02 gains a `msg_type` field and also fires for a frame with no order number and a foreign `Type`. After a live session, check whether `tv_order_update_hollow_decode_total` stays at 0.

## Guarantee matrix

Carries the 15-row + 7-row matrix by reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`; per-PR specifics are in each
PR body.


### Public operator portal Top Movers — 2026-10-31 continuation

Requested by the owner in the Codex continuation: add Top Movers to the supplied
API Gateway dashboard and verify the actual page. The prepared implementation
is undergoing validation; full validation and live deployment are separate steps.

Design: the existing operator authentication gate dispatches one fixed standalone
Rust reader through the existing SSM sender. The reader fetches the same server-side
API credential as app startup and calls only the fixed loopback F&O endpoint. It
preserves canonical prices and selects at most 20 gainers and 20 losers. The input,
transport, request time and per-container cache are bounded; this cold control path
does not claim constant time or submicrosecond latency.

Edge Cases / Failure Modes: missing helper, stopped instance, invalid credentials,
database failures, malformed or truncated responses, incomplete stock coverage,
stale data, redirect attempts, oversized streaming bodies, overlapping requests,
late replies after locking and background-tab expiry produce explicit states.
Conservative browser age includes request duration and two seconds for integer
timestamp precision. An upper estimate beyond the freshness limit is unconfirmed,
not proof that the source is stale; the newest trade may be outside displayed movers.
The canonical API now carries the UTC epoch of the same original observation used
to compute its row age. Both serialized API caching and the helper/Lambda bridge
must preserve that anchor: stamping helper completion could hide original query
time on a later cache hit. Missing or invalid legacy anchors fail closed. The
naive IST source clock must have its IST offset removed before becoming Unix time.

Rollback: revert the app helper packaging and operator Lambda together through the
existing deployment process. Missing or incompatible helper output fails closed.
The three-file swap is not atomic and existing automatic rollback restores only
the main app; no stronger rollback guarantee is introduced.

Observability: show captured time, conservative age, source state, priced/universe
counts and selected/total counts. Errors are typed codes; helper output never
contains credentials or raw SDK diagnostics. Actual live browser verification is
read-only and distinct from intercepted browser fixtures.

- [x] Connect the authenticated operator action to one standalone Rust read-only
  helper, with canonical app prices, bounded top20 selections, explicit errors,
  timeout, age and per-container cache.
  - Files: crates/app/src/operator_movers.rs, crates/app/src/bin/tickvault_operator_read.rs,
    crates/common/src/operator_movers_wire.rs, crates/aws-lambdas/src/operator_movers.rs,
    crates/aws-lambdas/src/operator_control.rs
  - Tests: canonical_large_universe_selects_complete_top_twenty_each_way,
    closed_is_preserved_with_no_invented_prices,
    malformed_or_inconsistent_source_is_refused_as_a_whole,
    movers_authentication_and_stopped_state_precede_any_private_request,
    movers_fixed_command_cache_expires_and_never_forwards_user_input,
    movers_concurrent_refresh_is_bounded_and_does_not_dispatch_twice
- [x] Add the accessible portal table with explicit closed/stale/expired/error
  states and package the helper through the existing deployment workflow.
  - Files: crates/aws-lambdas/src/operator_control_console.html,
    crates/aws-lambdas/src/operator_control_action_commands.rs,
    .github/workflows/deploy-aws.yml, crates/app/Cargo.toml,
    crates/common/tests/browser_surface_and_toolchain_guard.rs
  - Tests: console_embedded_shell_never_grows, ssm_workflow_shell_never_grows,
    workflow_run_lines_never_grow, javascript_bytes_inside_script_tags_stay_under_the_ceiling
  - Browser: real Chromium fixtures for rows, closed, errors, malicious symbol text,
    expiry, refresh in flight, lock, visibility and mobile layout; actual live
    portal re-check only after an explicitly reviewed deployment.
- [x] Validate the corrected source timestamp through API and Lambda caches.
  - Files: crates/api/src/handlers/fno_stocks.rs,
    crates/app/src/operator_movers.rs
  - Tests: test_one_row_is_live_with_change_and_value,
    test_get_fno_stocks_cache_hit_skips_the_query,
    cached_source_keeps_original_clock_instead_of_helper_completion_time,
    source_clock_missing_malformed_future_or_expired_is_not_replaced_with_now
  - Controls: re-anchoring at helper completion and omitting the IST conversion
    must fail their intended assertions; restore exact source afterward.
  - The former c5 workspace compile was deliberately interrupted before any
    tests after an independent cache-age counterexample; it is not a passed suite.
  - Corrected source: ten helper and twenty API tests passed before and after
    both effective fault controls. All fixed source hashes were restored;
    23 browser fixtures and 46 pure verifier checks passed. Full workspace,
    all-target lint, native publication and live deployment remain separate.
- [x] Reconcile existing deployment, crate-root and HTML guards with the new helper
  and requested tab; retain the exact content hash, ordered tabs and smoke-before-
  promotion-before-host-install assertions. Preserve the helper's production and
  test lint denials. No deployment or helper runtime behavior changes here.
  - Files: crates/app/tests/host_tuning_boot_wiring_guard.rs,
    crates/app/src/bin/tickvault_operator_read.rs,
    crates/aws-lambdas/src/operator_control.rs
  - Tests: boot_units_never_run_the_rollback_restored_binary,
    production_crate_roots_keep_the_deny_blanket,
    every_crate_root_decides_about_silent_panics,
    every_crate_root_denies_unsafe_code, console_html_content_pin,
    test_html_has_exactly_four_ordered_tabs
  - The unchanged bcf8dc7 diagnostic completed 15,445 passed / 6 failed / 33
    ignored across 460 harnesses. These six failures are preserved as failed
    evidence; the corrected candidate still requires its own validation.


### 2026-10-31: isolate the idle-seed and depth pre-size allocation gates

- [ ] Complete exact-source validation of the allocation-test isolation below.
  Source is prepared; this checkbox does not claim a passed suite or publication.
  - The 12f9632 full run completed 15,450 passed / 1 failed / 33 ignored across
    460 harnesses. The book pre-size case reached 16,000 levels, 1,600 ticks and
    966,368 book bytes, but process-global DHAT measured 6 blocks / 389 bytes
    against the unchanged 0-block / 0-byte ceiling. Preserve this failed result.
  - The test file is byte-identical on main, bcf8 and 12f. Pinned DHAT 0.3.3
    documents that a lock between test bodies does not exclude allocations by
    parallel libtest bookkeeping. The original failure has no saved stacks, so
    its specific allocation source remains unproven. Passing later nocapture
    diagnostics do not erase the captured full-suite failure.
  - Keep each of the three original heap cases in its own integration-test
    binary: dhat_idle_seed_step, dhat_depth_level_row_presize and
    dhat_depth_book_row_presize. Retain all original function names, bodies,
    warm-up, nonvacuity and zero-allocation assertions; no feature gating/skips.
  - Keep explicit CI commands, exact app target count 11, the zero-test refusal
    and sorted expected/actual target equality. Extract only the fixed app
    expected inventory into .github/dhat-app-targets.txt, with missing/invalid
    inventory refusal. Do not derive expected names from discovered files or
    raise the existing shell ceiling. Other crate lists/counts are unchanged.
  - Before final validation, run the predeclared five captured parallel
    stack-attribution diagnostics with temporary DHAT-aware assertions and
    retained profiles; restore exact source. If they all pass, attribution
    remains unknown. They are diagnostics, not replacement acceptance evidence.
  - Then run all three isolated positive targets, the book-reserve removal
    control (which must fail the allocation assertion after meaningful appends),
    exact restoration and final positives. Static/canonical guards, all-target
    lint, exact-commit full/native/CI gates remain required and pending.


### 2026-10-31: retain scanner verdicts within the existing native deadline

- [ ] Complete committed-head workspace/lint and native/CI validation after
  importing the independently reviewed shared scanner correction.
  - The portal ab5 workspace completed 15,451 passed / 0 failed / 33 ignored
    across 462 harnesses. Keep its source and result attribution intact.
  - Cache the existing test-file inventory per crate and use equivalent Bash
    name/prefix extraction. Preserve all matching expressions, exclusions,
    exemptions, baseline values, verdicts and existing 300/120-second deadlines.
  - Import the reviewed 14 real-hook Rust fixtures, including contained Git
    configuration/selector handling and the truthful assert-prefixed helper.
    Prior author/peer fixture and control evidence is not a new portal run.
  - Existing product Rust is byte-identical to ab5; the added common-crate
    fixture still requires the new committed-head workspace validation.
  - The 83.342-second full scan belongs to the earlier bd source observation.
    Do not relabel it as a portal timing or bypass any normal publication gate.

### 2026-11-01: diagnose and repair Portal #2178's actual coverage failures

The operator-authorized recovery coordinator requested this isolated successor on
`c2032ce28eb3c6c9be2fb2bbc742c37f246373da`; recovered-portal remains unchanged.
This item carries the existing 15-row + 7-row guarantee matrix by reference to
`.claude/rules/project/per-wave-guarantee-matrix.md`. The composed source retains
both independently reviewed corrections and their separately attributed evidence;
changed-head full/lint/native/CI validation remains pending at source freeze.

| Observed failure | Cause and preserved safety check |
|---|---|
| External Git sentinel changed in both coverage attempts | Detached fixture maintenance removed its own lock; keep every snapshot entry and the full equality assertion |
| WAL lifetime property overflowed on attempt two | Retained bucket ids collided after clock steps; require replay protection and retain strict no-overflow inside the collision-free envelope |

- [ ] Complete the Git fixture correction and changed-head validation without
  weakening external-repository isolation or the existing child deadline.
  - Files: crates/common/tests/pub_fn_test_guard_behavior.rs
  - Tests: fixture_git_children_cannot_mutate_a_poison_selected_external_repository,
    hook_child_cannot_false_pass_through_a_poison_selected_clean_index
  - Actual Git 2.56.0 with two libtest workers reproduced the original equality
    failure: the sentinel's empty `.git/objects/maintenance.lock` disappeared
    between snapshots. The fixed 20-run diagnostic matrix passed 19 runs and
    failed one; a separate matrix also caught disappearance during a read.
  - Keep complete snapshot equality, fatal read diagnostics, all Git-selector
    isolation and the existing ten-second child limit. Set only the synthetic
    fixture command's `maintenance.autoDetach=false` so automatic maintenance
    completes before snapshots. Do not exclude the changing lock file.
  - The exact corrected common source passed all 23 predeclared runs of the
    same 19 fixtures: Git 2.56.0/two workers for 20 runs, plus Git 2.52.0 with
    two/four workers and Git 2.56.0 with four workers. These are 23 repeated
    passing runs, not 437 distinct tests or a combined workspace/CI result.
- [ ] Complete the WAL property's retained-bucket envelope correction and
  changed-head validation with unchanged safety assertions and property bounds.
  - Files: crates/storage/tests/wal_unapplied_ranges_properties.rs
  - Tests: test_regression_portal_2178_in_session_overflow_seed,
    test_regression_portal_2178_exact_ring_wrap_replays_before_and_after_persist,
    proptest_wal_lifetimes_never_skip_or_prune_an_unapplied_frame
  - Exact input: seed 11217553172582376467, start 1, stuck false, mirror true,
    extra false, straddle false. Retain the original 256 property cases and
    all no-skip/no-prune assertions; no seed ignore or arbitrary bound change.
  - The isolated diagnostic reproduced the original assertion at lifetime 12,
    frame 0: retained id 6479807 and new id 6479935 share slot 63, exactly
    128 buckets apart. Production intentionally fails towards replay here;
    the production watermark implementation remains byte-identical to c203.
  - The R envelope uses actual retained ids and newly shed frame ids, including
    mirror and inverted records. Collision-free steps retain strict no-overflow;
    real collisions require finite known bounds and independent database-model
    no-skip/no-prune decisions in RAM and after persist/reload. Probes may
    legitimately persist an existing overflow into a protected range.
  - Preserve all six inputs, RNG draws, persist timing, 256 cases and existing
    Z/G/B/S/O/M3 assertions. The focused wrap fixture advances both sink HWMs
    beyond both pending marks so low watermarks cannot conceal lost protection.
  - The exact copied WAL source passed its complete ten-test property harness.
    Separately omitting the collision flag and bounded-range insertion each
    failed the intended replay assertion. Both exact restorations passed the
    focused regression again. These scoped results belong to the preserved WAL
    candidate; identical source justifies retaining them without new mutations.

Preserve run 41266407564 / job 124730630660 raw evidence. The first built-in
attempt failed the Git sentinel test; the second failed it again and the WAL
seed. Coverage threshold enforcement did not run. No resource-cause attribution
or blind rerun replaces these actual assertion failures. Original failure logs,
input packets, independent reviews and positive/control results remain retained.

Think of two alarms: keep both alarms working, record what set them off, then
check that each correction addresses its cause. The composed repair now requires
ordinary plan/common/property guards, a normal gated commit, exact new-head
workspace and all-target lint, then coordinated fast/native/pre-PR and CI checks.
No merge, deployment, coverage percentage or production latency result is claimed.


### 2026-11-02: preserve recovery and finish the requested Stocks surface

The user authorized continuation and parallel repairs, supplied the Stocks
reference screenshots, and asked that other fixes continue. Original unpublished
refs, missing-a9b0e89c evidence and prior validation records remain preserved.
Depth20, Depth200 and live order activation stay off.

- [x] Implement the existing portal Stocks view with F&O/index selection,
  watchlist, Gainers/Losers, full-universe backend sorting before paging,
  bounded search and truthful unavailable/stale/error displays.
  - Files: crates/aws-lambdas/src/operator_control_console.html,
    crates/aws-lambdas/src/operator_control.rs,
    crates/api/src/handlers/stock_movers.rs,
    crates/common/src/operator_stock_movers_wire.rs
  - Browser evidence: 28 passing Chromium controls on HTML SHA256
    4a86b97f90df048caf75f85470a9bdc8a630d9cf5c1efe4faf9aa642d78f270a;
    synthetic intercepted data only. Existing CSV functions remain preserved.
  - Exact portal byte measurement and bounded frontend scope are recorded in
    rust-only-forever-lock-2026-07-19.md §0.17; other allowances remain unchanged.
- [x] Correct the failures found by the combined native run without discarding
  their evidence or weakening the existing safety assertions.
  - Files: crates/common/src/source_scan.rs,
    crates/common/tests/production_panic_macro_guard.rs,
    crates/core/tests/order_update_choke_point_guard.rs,
    crates/core/tests/ws_telegram_visibility_guard.rs,
    crates/app/tests/cov_xvs_crossverify_spawn.rs,
    crates/app/src/dhan_live_crossverify_boot.rs,
    crates/app/src/daily_task_marker.rs,
    crates/app/src/holiday_gate.rs, crates/app/src/main.rs,
    crates/api/src/handlers/fno_stocks.rs,
    crates/common/tests/scanner_behavior_guard.rs,
    crates/common/tests/doctor_read_only_guard.rs,
    .claude/hooks/secret-scanner.sh
  - Tests: test_regression_production_region_cannot_borrow_another_items_test_gate,
    test_regression_panic_guard_requires_compiler_enforced_test_file_boundary,
    test_regression_choke_point_guard_sees_bypass_after_test_only_field,
    test_dhan_live_crossverify_boot_spawn_closed_calendar_never_pages_and_can_be_cancelled,
    test_missing_token_weekday_pages_once_with_test_write_marker_in_and_test_marker_exists_in,
    secret_scanner_preserves_all_families_and_comment_boundaries
  - The prior full run is 16,312 passed / 13 failed / 32 ignored; these are
    implementation corrections pending fresh combined native validation.
- [ ] Complete fresh combined workspace, all-target lint and required native
  guard/coverage validation; record exact scope and unresolved results.
  - Real owned QuestDB execution at f521fb09: five round-trip tests passed,
    then one restart readback passed using the same saved test binary.
    This establishes neither production durability nor power-loss safety.

Scenarios include long Unicode names at 320/390px, late/error/expired responses,
full-universe sorting, unknown fields, duplicate scanner inputs, test-only
fields before production code, fake cfg mentions, UTF-8 source boundaries and
paused Tokio timers with unchanged wall time. Native regression assertions stay
active. Rollback uses preserved refs/patches; no production deployment is part
of this local implementation checkpoint.


### 2026-11-02: close native publication-check and Rust-header gaps

The combined Stocks checkpoint 545d27149 completed 16,330 passing tests,
2 failing Rust-header classification guards and 32 ignored tests. Preserve that
failed result. The earlier 13 failures passed in both the focused and combined
runs; they are not the two failures in this checkpoint.

- [x] Distinguish compiler-defined Rust inner attributes from interpreter
  shebangs while retaining renamed-script detection, BOM handling and every
  existing runtime/scan allowance and non-vacuity assertion.
  - Files: crates/common/tests/rust_only_guard.rs
  - Tests: test_regression_rust_inner_attributes_are_not_interpreter_headers,
    test_regression_real_shebangs_remain_visible_when_scripts_are_renamed_rust,
    test_regression_only_rust_trivia_can_precede_an_attribute_bracket
- [x] Remove unproven shared fast receipts from the native pre-push decision.
  Every normal and FULL_QA invocation executes format, all-target Clippy,
  workspace tests, banned-pattern and test-count checks. Retain remaining
  checks and blocking failure/timeout outcomes; add --locked to build/tests.
  - Files: scripts/git-hooks/pre-push,
    crates/common/tests/native_pre_push_policy_guard.rs,
    crates/common/tests/shell_budget_guard.rs
  - Tests: default_and_full_qa_never_trust_shared_receipts_to_hide_failure_or_timeout,
    format_clippy_banned_and_count_failures_still_block_native_push,
    successful_native_push_runs_all_checks_and_only_then_writes_its_receipt,
    inherited_shell_startup_and_exported_functions_cannot_escape_the_owned_stubs
  - The workspace-test timeout is a fixed 1,800 seconds, based on the recorded
    1,288.604-second compile/test run; the old 300 seconds is below its
    529.85 seconds of harness runtime alone. This changes waiting capacity,
    not assertions, cases or passing criteria. Other timeouts are unchanged.
    Removal of receipt parsing removes its obsolete two-awk allowance.
  - Controlled fixture checks are not real native QA. Existing advisory-network
    and missing-tool skip policies remain explicit; actual audit and deny were
    separately executed successfully at 545d27149 with the pinned tools.
- [x] Address all-target lint findings in the new v2 Stocks response: box the
  large successful snapshot, retaining the identical serde wire schema and
  validation, and use an array in the three-row sort fixture.
  - Files: crates/common/src/operator_stock_movers_wire.rs,
    crates/api/src/handlers/stock_movers.rs
  - A successful snapshot adds one boxed allocation on the bounded control
    read path; no zero-allocation or submicrosecond claim is made.
- [ ] Execute the corrected native fixtures, all-target lint and full native
  publication checks at the new source checkpoint, followed by fresh coverage
  within measured resources. Preserve all earlier failed logs and compare
  source hashes rather than relabelling old evidence as a new full pass.

### 2026-11-02: preserve deployment artifacts and close publication blockers

The normal native push at77a3e1ac9 passed the full workspace, formatting,
all-target Clippy, advisory/dependency, Loom and integrity gates, then failed
the unchanged public-function test-recognition ratchet (322 versus307).
That push did not publish source. Its failed log remains retained.

- [x] Repair test discovery with meaningful function-name references and three
  new behavioral tests; leave the scanner and307 baseline unchanged.
  - Files: crates/api/src/handlers/stock_movers.rs,
    crates/app/src/operator_movers.rs, crates/app/src/subscribe_reconcile.rs,
    crates/app/tests/bin_subprocess_reports.rs,
    crates/app/tests/evening_state_recovery_boundaries.rs,
    crates/aws-lambdas/src/operator_control.rs,
    crates/common/src/operator_movers_wire.rs,
    crates/common/src/operator_stock_movers_wire.rs,
    crates/core/src/network/ip_verifier.rs,
    crates/core/src/test_support.rs, crates/core/src/test_support/ssm.rs.
  - Tests: test_matches_price_combines_case_insensitive_search_with_direction_and_missing_change,
    test_assert_requests_rejects_incomplete_misdirected_or_unencrypted_records,
    test_run_filtered_cli_rejects_bad_arguments_with_a_complete_v2_envelope.
    Each new test passed natively with unchanged source during its command.
- [x] Make the stopped-instance deployment fences fail closed on unknown,
  denied, timed-out or malformed reads; pin binary and checkout to one verified
  revision. Preserve prior fixed artifacts privately before overwrite and
  refuse dirty/untracked source or hidden index flags.
  - Files: .github/workflows/deploy-aws.yml,
    crates/app/src/bin/smoke_test.rs,
    crates/app/src/bin/smoke_test/deploy_backup.rs,
    crates/common/tests/aws_deploy_safety_guard.rs,
    crates/common/tests/aws_infra_wiring.rs,
    crates/app/tests/host_tuning_boot_wiring_guard.rs.
  - Edge cases: absent/past/current stop markers; denied, invalid calendar,
    oversized and timeout responses; bad revision and failed fetch/reset;
    source symlinks, replaced files, dirty indexes, hidden flags, collisions,
    bounded files/bytes/tool output, inherited Git repository selectors.
  - Test Plan:20 standalone behavioral tests and three planted regressions
    passed review; native smoke, deployment, infrastructure, host-tuning,
    shell-budget, Rust-only and budget-latch targets remain required.
  - Rollback: selected private prior artifacts and old Git ref are preserved;
    full automatic/atomic rollback is not claimed. Independently verify the
    temporary instance returns to its observed stopped baseline.
  - Observability: fixed failure categories and private checksum manifest;
    never emit marker secrets or file contents. Verify deployed provenance,
    effective activation settings and fresh read-only health after release.
- [x] Include both directly imported common mover-wire modules in the Lambda
  source digest, with executable-digest and omission controls. Trigger paths
  stay unchanged; actual deployed ZIP identity remains a separate check.
  - Files: .github/workflows/terraform-apply.yml,
    crates/common/tests/lambda_zip_bins_lockstep_guard.rs.
- [x] Archive #2169's historical handoff with byte-identical original payload
  and explicit historical provenance in docs/recovery/handoff-2026-10-31.md.
  Ratchet the static test-attribute count from15598 to16456. This is neither
  an executed-test total nor a coverage percentage.
- [ ] Run the final combined native gates, publish one reviewed integration
  PR, inspect exact-source CI and a real plan-only infrastructure run, use
  repository automatic merge, deploy both app and portal, and verify actual
  live behavior. Retain all earlier failures and separate old/new identities.

Live disk investigation also identified a unit error in the network alarm's
description. Correct only its wording and comments in
deploy/aws/terraform/alarms.tf: the configured Average NetworkOut is a byte
count per60-second period, with a50MiB threshold for five periods, not50MB/s.
Threshold, statistic, periods, actions and missing-data behavior are unchanged.
Current disk-use observations and earlier growth-rate alerts are distinct;
neither justifies deleting stored market data or suppressing the alarm.

The actual combined overlay checks passed117 common deployment/source guards,
30 smoke/host-tuning tests and8 Lambda budget-latch tests, each with unchanged
source during its command. The full public-function recognition guard now
passes at the unchanged307 baseline. Final committed-source native push and
CI remain required; these focused totals are not whole-workspace coverage.

- [x] Correct two demonstrated Stocks interaction faults in
  crates/aws-lambdas/src/operator_control_console.html: let the inactive native
  direction button own the entire visible switch hit area, and retain invalid
  UTF-8 search feedback using the existing typed error state during the1s
  repaint. Normal validation and lock clear that state; no invalid request or
  stale quote becomes usable.
  - Final HTML SHA2566226059a333a70a726bf4a34da7d4dc5c57818efd95951244bea42efab4d88da.
    JavaScript34,480bytes/308lines remains under the existing34,530/350 ceiling.
  - Exact-source Chromium:28 page scenarios,54 pointer/touch/keyboard switch
    scenarios and8 Data/CSV scenarios passed with intercepted synthetic data.
    The old source's timer defect and the first narrow hit-area edge failure
    remain recorded. Existing layout crops remain byte-identical.
- [ ] Reconcile newly discovered paused branch work beyond the24-PR inventory.
  Preserve all refs; current-source review has found additional AWS-control,
  token-cache, special-session and observability/storage proposals. Historical
  WIP is not accepted blindly: lagging rows refute an applied/queryable-row
  claim, but do not alone refute the documented acknowledged-WAL contract.

Depth20, Depth200 and live-order activation remain off throughout. Final
validation must not bypass gates, broaden runtime exceptions or relabel
after-close observations as open-market throughput/latency/loss proof.

### 2026-11-02: reconcile remaining branch work against the current runtime

Preserved all original branch refs and fetched the newer handoff2a4a916 under
a separate recovery ref. The full955-ref inventory and later remote-head
reconciliation distinguish represented changes, explicit retirements, unbuilt
proposals and ports requiring validation. Literal old branch tips are not
claimed merged merely because their behavior has a later equivalent.

- [x] Port private token-cache creation and safe stale-link replacement;
  protect QuestDB restart during capture and parse UTC08/09 as decimal.
  Files: token_cache.rs, aws-control.yml, aws_control_capture_guard.rs.
- [x] Use one IST session date and TradingCalendar::is_session_day for
  live-universe wait/widen eligibility, with executable regression controls.
  Files: main.rs, live_universe_session_day_guard.rs. This changes day
  eligibility, not the existing time windows or late-attach cutoff.
- [x] Report retained refused-spill bytes and actual overflow episodes;
  port the newer W4 parked-ack lineage including forgotten-table taint,
  persisted pending-state recovery and failed sidecar-write controls.
  Files: seal_spill.rs, wal_applied_watermark.rs, wal_suspension_watcher.rs,
  wal_unapplied_ranges.rs and the retained proptest regression seed.
  Contract: accepted QuestDB WAL acknowledgements plus the existing60-second
  writeback/settle and two clean probes. This is not proof of applied rows,
  an empty WAL backlog, fsync, power-loss durability or zero data loss.
- [x] Start existing telemetry after tracing and before watched supervisors;
  classify a failed thread spawn as HOT-PATH-04, Medium/log-only.
  Files: main.rs, error_code.rs, error-rules.yaml, runbook and wiring guards.
  Registered gauges alone do not prove the telemetry thread is running.
  Correct the stale options-store boot log: boot rehydrate was removed.
- [x] Port bounded local TCP/rmem pressure sampling, with a guard against
  broad CloudWatch export patterns. Files: kernel_rx_queue_sampler.rs,
  kernel_tcp_ext_local_only_guard.rs. Namespace counters do not prove
  per-connection delivery, loss or network latency.
- [x] Restore the observed enabled/disabled/enabled-runtime service state
  after filesystem recovery without starting it; reject unknown states and
  make restoration failure terminal across the outer SSH retry loop.
  Narrow scheduler PassRole to the actual scheduler role and retain the
  SSM service condition. Strengthen direct control-secret alias checks.
  Files: emergency-fs-recover.yml, main.tf, emergency_recovery_state_guard.rs,
  aws_infra_wiring.rs, github_workflow_guard.rs.
  Limits: SIGKILL/host loss and ambiguous SSH outcomes are not atomic rollback.
- [x] Port the production reader-memory allocation ratchet with cancellable,
  bounded fixture I/O, full payload/order checks and writer-task completion.
  Files: dhat_ws_reader_memory.rs and ci.yml. CI now explicitly executes10
  core DHAT targets. Preserve the48KiB idle,400blocks/MiB and4x held-payload
  ceilings, the existing293-line workflow budget and the existing coverage
  exclusion for dhat_ allocation tests. No new ignored tests or exemptions.
  Plain synthetic loopback excludes TLS/vendor behavior and latency claims.
- [x] Correct the full-console digest/length pin to the reviewed6226059a
  HTML84,855bytes. The prior6cf push was interrupted during tests after this
  mismatch was identified; no successful push is claimed for that attempt.
- [x] Ratchet the measured static test-attribute baseline16456 to16521.
  This is not an executed-test count, scenario completeness or coverage.
- [ ] Test Plan: full native publication gates and exact-source CI; explicit
  ten-target core DHAT run; storage failure/restart, auth inode, special-day,
  telemetry, workflow, source-budget and console-pin controls. Preserve all
  failed/interrupted attempts and report the actual final source identity.
- [ ] Review a real plan-only Terraform run, then automatic merge and deploy
  both EC2 binaries and operator Lambda. Verify full deployed SHA/ZIP/HTML
  identities, actual hosted Stocks selections and effective safety settings.
  Restore the observed stopped server baseline after temporary checks.

Current reviewed patches and standalone controls are saved under the
20261102T071741Z recovery evidence root. Native validation of this combined
port is still pending. Depth20, Depth200 and live orders stay off.

The first combined all-target Clippy run atab08101cb rejected the new
reader test's nested production-socket type. Name that existing type through
a private alias; do not suppress the lint or change its runtime behavior.
Retain the failed run and repeat the unchanged full lint gate before pushing.

### 2026-11-02: repair the two observed final-suite failures

The ordinary ff1cf12cf push failed its workspace test gate; all other native
gates passed. A complete no-fail-fast diagnostic run then recorded16,428
passed,2 failed,32 ignored with unchanged source. The failed targets were the
spawn-tool guard and the newly recovered reader-memory fixture. No push,
merge or deployment is claimed from that run.

- [x] Recognize GNU coreutils timeout only in the reviewed deployment backup
  module: at most7 literal sites, the same2 production deadlines and5 owned
  fixture children. Use the canonical production-region scanner and reject
  outside paths, lookalikes, extra sites, removed deadlines and trailing
  production. Runtime bans, backup code and its20/30s plus5s kill bounds are
  unchanged. Three new guard tests ratchet the static test floor to16524.
- [x] Initialize the reader fixture's explicit AWS-LC provider before DHAT.
  Warm each endpoint's distinct shared connector profile before the warmed
  per-socket comparison. Separately record first endpoint dials in process
  order and retained process state; Depth20 shares MainFeed's ALPN cache.
  The original127,312-byte Depth200 observation remains retained, not erased.
- [x] Replace positive net heap growth as a false proxy for FIFO ownership.
  Observe retained count, bytes and final marker/header/payload after the
  DHAT end sample, before releasing frames. A zero-growth positive control
  and missing-count/wrong-byte negatives pin this distinction. Preserve48KiB
  warmed idle,400blocks/MiB and4x incremental net-heap/payload ceilings and
  raw128KiB negative control. This is not total backing storage or RSS proof.
  A context-only diagnostic passed with the old assertion; the earlier
  failing client remains unattributed, so no raw-only cause is claimed.
- [x] The corrected two-target native run passed22 tests with0 failures and
  unchanged source. Two independent reviews bind the reader correction to
  SHA25624cb14efd8cbf7518deed0cfd4ef813805066d16c4cfbfa2d086a2ef9a202b2c.
- [ ] Re-run final committed-source native publication, exact-source CI and
  the real infrastructure plan before automatic merge and live deployment.

All changes in this correction are tests or records. Keep both depth feeds
and live orders off. Earlier failures and every unpublished checkpoint remain
preserved; finite checks do not establish universal reliability or latency.


### 2026-11-03: complete publication and close the dated budget/fence defects

The user continues to authorize fixing, building, merging and deploying the
recovered work. Keep every original ref and unpublished patch. Both depth
feeds and live-order activation remain off. The November 2 native push at192
passed the full workspace, lint, audit/deny, both Loom suites, integrity and
financial checks, but the public-function scanner exceeded its120-second
native limit. An isolated original scan took168.532 seconds and counted306
against baseline307. The first proposed optimization also timed out; retain
that failed record and do not claim publication or change the deadline.

- [x] Apply the documented November standing native budget of75USD. Quotes37
  and45 already limit185USD to October. Actual AWS still read185USD onNov2.
  Preserve historical185 defaults and the existing UTC-month runtime clamp;
  compare native budget with effective November-onward values in six guards.
- [x] Make both Terraform stop-marker reads refuse unknown/malformed values.
  Accept absence only from classified ParameterNotFound, preserve provider
  status and bytes, bound response/time and keep raw output private. Preserve
  the held-current-month false override and known absent/nonmatching default.
- [x] Apply the same byte-preserving capture to the deployment start fence.
  Extra/oversized trailing newlines and multibyte errors cannot normalize to
  a valid marker. Keep20+5second/4097byte bounds, parameters and shell budgets.
  Earlier V1 counterexamples are saved; independent reviewers passed corrected
  controls. These are infrastructure controls, with no new runtime language.
- [x] Combined native budget, deployment, infrastructure, workflow and shell
  guards passed137 tests,0failed,0ignored on the applied three-patch source.
- [x] Finish a semantic-preserving public-function scanner optimization.
  Retain the legacy declaration/test regex, line boundaries, duplicate counts,
  crate scope, exemption adjacency, unusual-file fallback and missing-state
  refusal. Retain Bash3-compatible syntax and native120 seconds. Final hook
  SHAe12b5307 passed34 equivalence cases and independent binary/error controls.
  Saved-source scan completed21.378s; actual combined-tree scan completed33.255s
  with unchanged source/count306. Ratchet the measured untested floor307 to306
  and the static test-attribute count16524 to16543; these are not coverage.
  - Files: .claude/hooks/pub-fn-test-guard.sh, crates/common/tests/pub_fn_test_guard_behavior.rs
  - Tests: indexed_exemptions_preserve_adjacent_markers_and_unterminated_last_line,
    indexed_test_lines_keep_utf8_and_separate_file_boundaries,
    indexed_test_corpus_falls_back_for_early_late_and_mixed_binary_text,
    large_binary_inventory_preserves_matches_when_zero_check_stops_early
- [x] Add the missed15Jan2026 holiday from official NSE/CMTR/72260 and
  NSE/FAOP/72262. Exercise the shipped calendar: Jan15 closes while Jan14/16
  stay open. All other configuration is unchanged. The60-day expiry reminder
  remains active; no authoritative2027 circular was verified or invented.
  - Files: config/base.toml, crates/common/tests/nse_holiday_calendar_guard.rs
  - Tests: test_2026_election_addendum_closes_shipped_calendar
- [x] Combined native scanner/calendar, Rust-only, browser/toolchain, shell
  and ignored-test guards passed111 tests,0failed,0ignored at03:41UTC.
  Preserve the earlier18pass/1fail forbidden grep-spawn and110pass/1fail
  filesystem-order runs. The fixture now uses Rust filesystem enumeration and
  places its binary match in the observed first entry, with order checked
  again after overwrite. No spawn allowance or deadline was increased.
- [ ] Run final native guards, measured static-count ratchets, committed-source
  all-target lint and ordinary publication. Preserve every failed attempt.
  Create and attach the draft PR only after the actual fresh pre-PR gate.
- [ ] Accept exact-source CI, dedicated allocation checks, raw coverage report
  and a real plan-only Terraform run before the repository merge flow. Review
  daily-start state, native75 budget, IAM and all Lambda changes. Keep plan/apply
  stop-marker refusal active; do not infer absence from permission errors.
- [ ] Verify deployed app/helper and portal identities, real Stocks behavior
  and effective activation settings. Use today's observed instance baseline
  and market/capture window, never yesterday's stopped assumption. Preserve
  private deployment backups and live data; app rollback must not restore the
  expired October native budget or undo a valid budget/holiday stop marker.

Package all recovered Codex work and compatible pending PR changes in one
combined PR, as explicitly requested onNov3. Map all24 original PRs, preserve
their original refs, and resolve covered PRs only after verified combined
landing. The incompatible parking_lot0.13 upgrade remains explicit. The exact
GitHub-token SSM parameter is absent in current metadata; the ancestry repair
does not provision credentials. No speculative IAM permission change is made.

Observability: keep source hashes, command outcomes, actual workflow heads,
plan actions, selected live safety fields and actual screenshots. Error paths
report typed failure without provider bodies. No finite test set establishes
zero bugs/loss, exhaustive coverage or universal O(1)/submicrosecond latency.

### PR MV — 2026-11-06 live incident: portal Stocks movers fast path (common, api, app, aws-lambdas, terraform, portal)

**Approved by:** Parthiban, 2026-11-06: "go ahead fix everything dude approved" / "go ahead fix everything dude dont stop".

**Measured (live, 2026-11-06 10:59 IST):** the movers SQL takes 0.11–0.37 s and the box-side `tv-movers` helper 0.12–0.33 s, but every portal click goes portal → Lambda → EC2 DescribeInstances → SSM SendCommand → agent → helper → SSM poll every 400 ms, and the operator-control Lambda ran 2.6–8.0 s per call (CloudWatch REPORT lines). The Lambda cache holds one filter for 20 s, so a sort, page or filter click pays the whole trip; the page blanks its table while it waits; a second click while one is in flight returns Busy.

#### Design
- Shared pure selection: the per-filter selection (`finish` and the list/status branching in `compute`) moves into a common module as `select(universe, filter) -> Reply`, and `/api/stock-movers` itself becomes `build_universe` (one QuestDB query over the union of the F&O list and the index-membership list) then `select`, so the API and the portal answer from one code path.
- The app publishes the universe ON DEMAND (review 2026-11-06: no standing QuestDB load): every 5 s it reads the Lambda's viewer heartbeat `movers/viewer.json` and builds + publishes only while it is unexpired (written on a live movers call, at most once per 30 s per container, valid 120 s); after the close the rows are queried once per (IST date, window end) and re-stamped; no query while QuestDB reports a suspended table or a growing apply backlog; failures back off 5 s × 2^n up to 120 s; every request names the expected bucket owner. Destination `s3://tv-<env>-live-view/movers/universe.json` (new bucket: no versioning, public access blocked, SSE-S3, objects expire after 2 days; the cold bucket is not used because it is versioned and may never expire noncurrent versions).
- The Lambda reads that object (2 s timeout) for a live-view filter and selects in memory; a missing, unreadable or stale object (older than 20 s, open or closed, or built in the other session state than the Lambda's own clock), or any `as_of` filter, falls back to the existing SSM path unchanged.
- The portal keeps the previous rows visible while a refresh runs.

#### Edge Cases
Empty or partial membership/F&O lists; index filter naming an index absent today; `fno_only` with no F&O list; market closed before 09:15; a publish in flight across 15:30/15:40; clock skew between box and Lambda; object larger than the wire bound; a universe from a previous IST day; duplicate security ids; NaN/zero prices; offsets past the end.

#### Failure Modes
S3 unreachable from the box (publish counted and skipped, portal falls back to SSM); S3 unreachable from the Lambda (fallback); corrupt object (rejected, fallback); stale object (fallback); QuestDB slow (publish times out, previous object ages out, fallback). Nothing on the tick path changes.

#### Test Plan
Property test: for random universes and random filters, `select` over the published universe equals the reply the old per-filter `compute` produced (old code kept under `#[cfg(test)]` as the oracle). Unit tests for staleness, day rollover, fallback, oversize. Lambda tests with a fake S3 shell. Terraform shape guard for the bucket (no versioning, PAB, lifecycle, IAM scoped to one key).

#### Rollback
`[operator_movers] publish_enabled = false` stops the publisher; the Lambda then always falls back (object ages out). Reverting the PR restores the old path.

#### Observability
`tv_movers_universe_publish_total{outcome}` and `tv_movers_universe_publish_duration_ms` (local /metrics only, no page); the Lambda logs `source=s3|ssm` and the elapsed ms per movers call.

- [x] MV-1 shared `select` in common + API refactor (identity proptest)
  - Files: crates/common/src/operator_movers_universe.rs, crates/api/src/handlers/stock_movers.rs, crates/api/src/handlers/fno_stocks.rs
  - Tests: proptest_select_equals_the_per_filter_path, test_select_equals_the_per_filter_path_on_named_edge_cases, test_select_filters_sorts_and_pages_one_universe_for_every_filter, test_universe_constants_match_the_common_constants
  - Review fixes (2026-11-06): one clock reading (`LiveClock`), security-id tie-break in both sorts, two-slot list caches, file loads and body parse on the blocking pool, constants pinned in the api crate. Tests: test_live_clock_takes_session_state_and_time_from_one_instant, test_is_trading_session_at_reads_the_given_second_and_its_ist_date, test_file_cache_keeps_today_and_one_other_date, test_universe_constants_equal_the_fno_stocks_constants, test_usable_at_refuses_other_day_stale_future_and_session_changed_universes, test_weekday_session_open_at_follows_the_ist_clock_and_weekday
- [x] MV-2 app publisher task + config switch + counters
  - Files: crates/app/src/movers_universe_publisher.rs, crates/app/src/main.rs, crates/common/src/config.rs, config/base.toml
  - Tests: test_encode_refuses_failed_queries_and_invalid_universes, test_failure_episode_warns_once_and_reports_the_recovery, test_operator_movers_publish_defaults_on_and_parses_overrides
  - Review fixes (2026-11-06): on-demand publishing on the viewer heartbeat, backoff, closed-read reuse, QuestDB health hold, expected bucket owner, bucket-name validation, failure reason on the warn. Tests: test_no_active_viewer_builds_nothing_and_never_queries, test_an_active_viewer_builds_and_publishes_unless_questdb_is_busy, test_after_the_close_one_query_serves_the_evening_until_the_key_changes, test_next_interval_doubles_from_five_seconds_to_a_two_minute_ceiling, test_failures_reset_on_success_and_no_viewer_and_hold_on_questdb_busy, test_questdb_quiet_needs_a_measured_healthy_reading, test_publisher_target_validates_the_bucket_name_and_fails_closed, test_account_id_is_read_from_the_identity_document_and_validated, test_read_bounded_refuses_an_oversize_body_without_a_length, test_closed_read_key_names_the_date_and_window_end_of_a_closed_read_only, test_restamped_moves_only_the_observation_and_only_for_the_same_closed_read, test_viewer_heartbeat_written_at_parse_and_active_at, test_valid_live_view_bucket_name_and_valid_account_id
- [x] MV-3 terraform live-view bucket + two scoped IAM grants
  - Files: deploy/aws/terraform/live-view-bucket.tf, deploy/aws/terraform/main.tf, deploy/aws/terraform/operator-control-lambda.tf
  - Tests: test_live_view_bucket_is_unversioned_expiring_private_encrypted_and_tls_only, test_live_view_grants_name_one_object_per_action_only (review: heartbeat grants, BucketOwnerEnforced, abort incomplete multipart uploads)
- [x] MV-4 Lambda S3 read + fallback + timing log
  - Files: crates/aws-lambdas/src/operator_stock_movers.rs, crates/aws-lambdas/src/operator_control.rs, .github/workflows/terraform-apply.yml
  - Tests: test_stock_movers_answer_from_a_fresh_published_universe_without_ssm, test_from_published_refuses_every_unusable_universe_and_the_box_answers, test_from_published_times_out_and_the_box_answers
  - Review fixes (2026-11-06): heartbeat write, session-state refusal, bounded body read returned as Bytes, expected bucket owner. Tests: test_a_live_movers_call_writes_the_viewer_heartbeat_once_per_interval, test_claim_heartbeat_once_per_interval_and_after_a_clock_step, test_live_view_target_refuses_a_bad_bucket_or_owner, test_read_body_bounded_refuses_over_the_limit_and_keeps_one_chunk_whole
- [x] MV-5 portal keeps rows while refreshing
  - Files: crates/aws-lambdas/src/operator_control_console.html
  - Tests: console_html_content_pin
  - Review fix (2026-11-06): the table is inert and dimmed and Previous, Next and Refresh are disabled while a refresh is in flight.

### PR DP — 2026-11-06 live incident: the disk-pressure archiver starved QuestDB 08:31–09:13 IST (app, storage, deploy)

**Approved by:** Parthiban, 2026-11-06 ("go ahead fix everything dude approved").

**Verified root cause (live logs, QuestDB SHOW PARAMETERS, errors.jsonl):** the box booted at 95% disk used; the disk-pressure leg exported 41 partitions back to back 08:31:05–09:13:11 (nine market_depth hours of 95–115M rows, 4–5 min each) through QuestDB HTTP /exp. QuestDB runs one shared network worker (`shared.network.worker.count=1`, `http.worker.count=0`) on one core, so each export held the thread every ILP /write and /exec needs: 14 BOOT-01, 30 reconnects, 2,935,971 tick rows and 2,331,360 depth levels rescued to spill, WAL probe blind x4. Underneath: `market_depth_book` exists with designated timestamp `timestamp` and 0 rows, so array rows are refused and inline depth writes 10 level rows per Full packet (~7 GiB/h); the Nov 5 pressure episode spent its 3 re-arms by 11:37 and held all night at 92%; the after-close depth pass stopped ~700 times at its 8% free floor while the drain hold kept the box up to 07:30. Same pattern Nov 4 and Nov 5. Ruled out: boot WAL replay (refused), evening restart (none), #2191, CPU quota throttling.

#### Design
- QuestDB gets its own HTTP worker pool (`QDB_HTTP_WORKER_COUNT=3`, review M6: export limit 4 → 2, workers = export limit + 1) so an /exp stream cannot hold the thread ILP /write and /exec use; host tuning stops forcing the network pool to 1.
- The pressure pass is gated: inside 08:45–15:40 IST it exports no market_depth partition unless used ≥ 90%, and exports at most one partition per in-session pass; a pass started before the open stops at its next partition boundary after 09:00.
- The pressure episode re-arm budget resets when the IST trading date changes or the session closes, and (review M3) after the 15:40 gate and on no-session days an escalated episode re-arms every poll with no cap and refunds passes that drop partitions, so 15:40 to the 16:30 stop reclaims back to back; the box is NOT kept up for it (cost lock).
- The after-close depth pass that stops at its disk floor requests a reclaim, and the drain hold no longer counts floor-blocked depth buckets (a floor-blocked hold is capped at one hour and counted).
- Boot self-heal: when `market_depth_book` exists with the wrong designated timestamp AND `count()` is 0 AND its WAL is applied AND `sequencerTxn` is unchanged around the count AND the depth spill replay is held (08:00–15:40 IST), RENAME it to `market_depth_book_lh01_<yyyymmdd>` (nothing ever deleted) and re-create it with `ts`; anything else keeps today's refusal (review fix: the first draft DROPPED it, racing the depth replay).

#### Edge Cases
Pressure at ≥ 90% inside the session; a partition export already running at 09:00; trading-date change while an episode is open; holiday and weekend; non-empty wrong-ts book table; QuestDB unreachable during the self-heal; HTTP worker count on a 1-core cpuset.

#### Failure Modes
Disk fills inside the session below 90%: the ingest shed steps (85%/92%) take their trigger from an escalated episode, and a gated in-session episode now escalates after five refunded passes with used% not falling (review M1; before, every gated pass was refunded and the shed lost its trigger), alarms unchanged; export refused (counted, retried next pass); self-heal rename fails (falls back to level rows exactly as today, coded warn).

#### Test Plan
Unit tests for the gate (time × used% × table), the re-arm reset on date change, the drain-hold exclusion, the self-heal guard (0 rows only), a compose/host-tuning guard that the HTTP pool is separate. Scoped `cargo test -p` for app and storage.

#### Rollback
Revert the PR; the compose line is one deletion.

#### Observability
`tv_disk_pressure_pass_gated_total{reason}`, `tv_disk_pressure_rearm_reset_total`, `tv_depth_book_self_heal_total{outcome}`, `tv_drain_hold_floor_blocked_buckets` (local /metrics only, no new page).

- [x] DP-1 QuestDB HTTP worker pool + host tuning
- [x] DP-2 in-session gate + one partition per pass + pre-open deadline
- [x] DP-3 re-arm reset on date change / session close
- [x] DP-4 after-close floor → reclaim request; drain-hold exclusion and cap
- [x] DP-5 market_depth_book zero-row self-heal: RENAMED aside (`market_depth_book_lh01_<yyyymmdd>`), never dropped, only inside the depth replay hold with an unchanged `sequencerTxn` around the count (review fix)
- [x] DP-7 after-close depth pass floor: 8% (12.9 GB on the 150 GB box, where the disk sat all night: 795 early stops 15:45 Nov 5 → 05:35 Nov 6) replaced by one point above the 5% pinned-delete floor plus a 2 GiB byte floor, after asking for a reclaim and waiting up to 3 minutes; stale 644 GB comment fixed (floor value superseded by review M5 below)
- [x] DP-8 spill replay retires the LAST spill file too once its hour has closed and it has been quiet (`spill_file_may_still_be_appended`); before, `depth-dhan-498258.dspl` (2.6 GB, 3 Nov) was replayed every evening
- [x] DP-9 the pressure-episode log text and the `pressure_hot_days` config comment say that market_depth HOURS (yesterday's included) are eligible; only DAY partitions keep the today+yesterday floor
- [x] DP-10 the re-arm budget also refills when a session depth hour passes its hot window (`rearm_window_key`), not only at the date change and the close; config comment carries the measured ~45 GB/day DB + ~20 GB WAL on 150 GB
- Implemented 2026-11-06 on branch `dp-2026-11-06`: `disk_pressure::{pressure_pass_limits, rearm_window_key, refresh_rearm_budget}`, `partition_archive::{ArchivePassLimits, apply_pass_limits, pass_deadline_reached}`, `disk_pressure_boot` (gate, refill, GatedProgress), `depth_persistence::self_heal_empty_market_depth_book`, `tick_spill_replay::spill_file_may_still_be_appended`, `deferred_depth_pass::disk_stays_low`, `drain_hold::DayWork::deferred_floor_blocked`, `hot_path_telemetry::record_stage_nanos_weighted`, `host_capacity_minute::record_process_core_count`, `ensure_questdb::QUESTDB_ENV`, `questdb_compose_env_guard.rs`. Deviation: the pre-open pass stops at its next partition boundary after **08:45** IST (the gate start, per the coordinator brief), not 09:00 as the Design line says.
- [x] DP-6 telemetry: `HostSampler` reads the core count before the background pin (it read 1 on a 2-core box, so `process_cpu_pct` is a share of one core); `hot_path_latency_minute` gets a per-tick figure for `drain_main_frame` (an eighth, never-paged stage `drain_main_tick`: each main-feed frame's drain time divided by its tick packets, recorded with weight = tick packets through the O(1) `record_stage_nanos_weighted`, DHAT-gated; a per-tick AVERAGE within a frame, and since 1,024 ns is a bucket edge, `p99_ns <= 1024` means under 1 µs) and writes NULL for `samples_over_1024ns` on the lag, dwell and receipt stages, where it is 100% by construction; runbook note that `ts` is IST written as Z (compare with `dateadd('m', 330, now())`)

##### PR DP review fixes, 2026-11-06 (three reviewers: hot-path, security, hostile)

- [x] H1 a gated (capped) pressure pass drops every table the run-start `wal_tables()` probe found behind BEFORE its cap, the cap counts partitions actually EXPORTED, and a gated pass whose only problem is `wal_not_applied` classifies StoodAside (before: the cap took a ticks day still being written, read Dropped(0) and paged a false STORAGE-GAP-05) — `partition_archive::{wal_lagging_tables, drop_wal_lagging, ArchiveRunSummary::pass_gated}`, `disk_pressure_boot::classify_pressure_pass`
- [x] M1 refunded passes (gated progress, or an in-session stand-aside at 90%+) that do not lower used% five times in a row set `EpisodeState::gated_stalled`, which escalates — `disk_pressure::{GatedStallTracker, GATED_STALL_PASSES}`
- [x] M2 a floor stop by a pass that wrote rows or cleared a bucket restarts `FLOOR_BLOCKED_SINCE_UNIX` — `deferred_depth_pass::next_floor_blocked_since`
- [x] M3 after the 15:40 gate and on no-session days: no re-arm cap (`lift_rearm_limit`), passes that drop partitions are refunded (`refund_off_session_progress`); box NOT kept up longer; base.toml comment corrected
- [x] M4 the 3-minute floor wait runs BEFORE the depth-partition hold and only inside the window; under the hold the floor is read without waiting — `deferred_depth_pass::run_after_close_pass`, `wait_for_run`
- [x] M5 after-close floor = `max(5% line + 7 GiB rewrite + 2 GiB, 10 GiB)` (`after_close_floor_bytes`, ~16 GiB on 150 GB); the reclaim is woken only above the 5% line (`space_request_safe`)
- [x] M6 `QDB_HTTP_EXPORT_CONNECTION_LIMIT` 4 → 2, `QDB_HTTP_WORKER_COUNT` 2 → 3; guard pins workers > export limit; `ensure_questdb` mirror follows
- [x] M7 a stood-aside in-session pass reruns only when the re-arm key moves or used% crosses 90 (`in_session_rerun_skippable`); the raw uploader is woken in session only at 90%+ (`raw_upload_wanted`)
- [x] L1 the gate reads the calendar (`pressure_session_day`: `is_session_day`, Muhurat window from `muhurat_session_secs`), and a pass started after the gate or on a no-session day stops at the next session day's gate (`OffSessionDeadline`)
- [x] L2 the fallback `docker run` passes a cpuset only when `TV_QDB_CPUSET` is set; an IPv6 bind is bracketed in the `-p` spec
- [x] L5 `drain_main_tick` docs: the frame time includes non-tick packets, and a green tile is the 1,024 ns bucket, not ≤1,000 ns; frame duration computed once; `PROCESS_CORE_COUNT` read before `available_parallelism`; stale "seven a minute" and spill-replay comments fixed

### PR AL — 2026-11-06 live incident: alarm truth and retries (app, storage)

**Approved by:** Parthiban, 2026-11-06 ("go ahead fix everything dude approved").

**Verified:** `tv_dhan_ws_lag_max_ms{class="fno"}` read 4,038,655 ms (67 min) while `{class="index_spot"}` read 1,923 ms and steady hand-off timings are microseconds: an illiquid option's old last-trade time is counted as delivery lag, so `dhan-feed-delay-high` stays in ALARM all session. SCOREBOARD-01 fires daily because the boot DDL and the 15:45 rollup make one 10 s attempt against a stalled QuestDB and never retry (5 Nov `ws_connection_daily` / `feed_scoreboard_daily` rows missing).

#### Design
- Feed-delay worst-lag recorder counts a packet only when that contract's trade time moved forward since its last packet (a new trade), and never the first packet after a subscribe.
- Scoreboard and ws-connection rollups retry every 5 min until success or 16:20 IST; boot DDL for the scoreboard and episode tables retries with backoff instead of one 10 s attempt.
- Backfill 5 Nov with the existing `TICKVAULT_SCOREBOARD_DATE` re-run after deploy (operator step, idempotent).

#### Edge Cases
A contract whose first packet carries a trade newer than subscribe; counter restarts; repeat quotes routed to tick_quotes; rollup succeeding on the last retry; process stop during a retry.

#### Failure Modes
QuestDB down all evening: rollup gives up at 16:20 with one coded error (as today), nothing worse.

#### Test Plan
Unit tests: lag recorder ignores non-advancing trade times and first packets; retry schedule; DHAT unchanged on the drain.

#### Rollback
Revert the PR.

#### Observability
`tv_scoreboard_rollup_attempts_total{outcome}` local only; no new page.

- [x] AL-1 feed-delay recorder counts new trades only
- [x] AL-2 scoreboard / ws-connection rollup + boot DDL retries
- [x] AL-3 (needs a dated noise-lock row first) questdb-disconnected 3-of-5 datapoints; risk-gap-03 period 300 s

#### Owner decisions, 2026-11-06 (decision card, recorded verbatim)
- Feed-delay page: **"Index feed only (Recommended)"** — page only when the index/spot class is over 60 s late; stock options keep a dashboard-only number; count only real new trades. (Noise-lock §2.6 row edited in the same PR, rule-file-first.)
- Noisy alarms: **"Yes, quieter (Recommended)"** — questdb-disconnected fires on 3 bad minutes out of 5; RISK-GAP-03 clears 5 minutes after the condition ends (period 300 s). (Noise-lock dated row in the same PR.)
- Main-feed depth data: **"Keep, compact form (Recommended)"** — keep saving it, as 2 book rows per packet (DP-5).
- 1 µs gate: **"Honest 2.5 µs for now (Recommended)"** — Full-packet frames get their own measured 2.5 µs key; Quote frames keep 1 µs; lower it as the code improves.

- [x] AL-4 feed-delay alarm re-pointed to the index/spot class gauge (EMF selector + live-lane-alarms.tf), noise-lock §2.6 dated row
- [x] AL-5 RISK-GAP-03 late-attach noise: the live lane's silence detector (`DetectorConfig::live_lane`, crates/core tick_gap_detector + crates/app dhan_feed_stack) gives a seeded `NSE_FNO` contract a 10-minute warmup and floors its judged cadence at 60 s; indices and cash stocks are judged exactly as before (dead-class rollup not delayed). Tests: `test_regression_a_late_attached_sparse_option_is_not_judged_for_ten_minutes`, `test_in_attach_warmup_never_applies_to_an_index_or_a_cash_stock`, `test_index_and_cash_stocks_are_judged_as_before_from_the_first_scan`
  - Verified (2026-11-06 live, measured on the box, NSE_FNO, 10:00-11:00 IST, about 10,329 contracts, ticks plus tick_quotes): median 207 packets an hour (about 17 s apart), p10 107 an hour (about 34 s), and the quietest contracts sent 1 to 3 packets in the whole hour (minimum 2 an hour, 3 contracts). So the Dhan "every ~30 s" assumption is false for the quietest contracts. In the live lane a stock option that has ticked never counts toward RISK-GAP-03 through silence (judged sparse on the 60 s floored cadence); a contract that only sent its subscribe snapshot never counts; only a seeded option that sends nothing at all past the warmup still counts. Tests: `test_a_one_packet_per_40_minutes_contract_never_counts_toward_risk_gap_03`, `test_a_contract_that_only_sent_its_subscribe_snapshot_never_counts`
- [x] AL-6 holiday-calendar reminder noise: `HolidayCalendarCoverageLow` pages at most once per ISO week while more than 14 days of coverage remain (week persisted in `data/state/calendar-staleness-week`, temp file + rename), every boot at 14 or fewer, run out or empty; gauge every check (crates/app calendar_staleness). Tests: `test_more_than_14_days_left_pages_once_per_iso_week`, `test_14_or_fewer_days_run_out_or_empty_page_every_boot`, `test_last_paged_week_round_trips_through_the_file`, `test_a_damaged_week_file_reads_as_not_paged`
- Note (AL-2): only the same-day run retries (`is_same_day_run`); a past-day `TICKVAULT_SCOREBOARD_DATE` backfill makes one attempt. DDL retries stop after one statement exhausts its budget (later statements one attempt each). Tests: `test_run_scoreboard_ddl_statements_*`, `test_exec_scoreboard_ddl_with_retry_*`, `test_rollup_retry_delay_secs_*`
- Review fixes (2026-11-06, three reviewers): AL-2 retries the WHOLE attempt (DDL, reads, build, write); every QuestDB failure before the final attempt logs `info!` with no code and returns before anything else is written; at most 8 attempts and none after 16:20 IST (`rollup_attempt_is_final`); zero appended rows is never counted `ok`. AL-1 `advanced` also excludes the replay hand-over gap packet; its exclusions count under reason `not_new_trade`. AL-5 keeps the detector verdict RAW (subscribe reconcile and the dead-class rollup read it) and gates only the page through `SilenceReport::warming_up` and `alarm_sparse`; the warmup starts at the first of seed or first packet. AL-6 week file: regular-file check, 2 s read timeout, exclusive 0o600 temp file. RISK-GAP-03 honest limit recorded in noise lock §2.16. Tests: `every_questdb_failure_in_the_attempt_is_quiet_until_the_final_attempt`, `test_rollup_attempt_is_final_caps_at_eight_attempts_and_the_deadline`, `test_regression_advanced_is_false_on_the_packet_that_ends_the_hand_over_gap`, `test_warmup_never_rewrites_the_raw_verdict_for_reconcile_and_the_class_rollup`, `test_a_warming_up_never_ticked_contract_still_reads_missing`, `test_regression_a_contract_that_ticks_before_its_seed_still_gets_the_warmup`

### PR CI — 2026-11-06: nightly CI truth (workflows, benches)

**Verified:** full-test-nightly fails on "No space left on device" while linking ~159 app test binaries with full debuginfo (no test assertion failed; 8 nights red); the aarch64 job is killed by runner shutdown; the bench gate fails Full-packet frames at 1.5–4.6 µs/tick against a 1 µs key that was set from Quote frames; `order_path_oms_order_update_fill` failed +8.3% (10 ns on 128 ns) with no code change since its baseline.

- [x] CI-1 nightly test jobs build with `CARGO_PROFILE_DEV_DEBUG=0` / `CARGO_PROFILE_TEST_DEBUG=0`, docker prune and a `df -h` before/after (x86 and aarch64) — `.github/workflows/full-test-nightly.yml` (job env, prune + `df -h /` trap, duplicate `cargo check` step removed), `github_workflow_guard.rs::nightly_builds_without_debug_info_and_keeps_one_issue`, `shell_budget_guard.rs` ceiling 83 → 60
- [x] CI-2 nightly updates one open issue instead of opening a new one each night, and prints the failing test names — one step "Report regression on the one open issue" (search, comment, else create; `grep -m 60` of FAILED/panicked/error[/No space left first)
- [x] CI-3 `feed_drain_main_feed_full` = 2,500 ns key for Full-packet frames (owner card above); Quote frames keep `feed_drain_main_feed` = 1,000 ns; CLAUDE.md key count updated — `quality/benchmark-budgets.toml` (key above `feed_drain_main_feed`; Full ids renamed `feed_drain/main_feed_full_frame_4[_own]`), `drain_tail_gate::MAIN_FEED_FULL_DRAIN_BUDGET_KEY` + `test_real_budget_file_routes_full_and_quote_ids_to_their_own_keys`, `benches/feed_drain.rs::full_feed_drain_tail_gates`, `feed_drain_full_frame_evidence.rs::test_full_and_quote_tail_gates_read_their_own_budget_keys`, CLAUDE.md 37 → 38 keys
- [ ] CI-4 finish and ship the never-opened `claude/assurance-fix-w4` (confirm parked live acks, W4) as its own PR after these
  - 2026-11-06: the W4 code is already on main (#2187); the branch is superseded, not cherry-picked (see W4). What is left to ship is the W4 tick and its CLAUDE.md O(1) row (branch `w4-2026-11-06`); tick this once that merges.

### PR TG — 2026-11-06: tests for the incident's worst-case combinations (storage, app)
- [x] TG-1 slow-QuestDB boot: the six audit/rollup ensures retry, DEDUP keys reach the table before the first ILP row (`cov_audit_ddl_slow_boot.rs`)
  - Files: crates/app/tests/cov_audit_ddl_slow_boot.rs (local stand-in QuestDB; no production change)
  - Tests: `test_regression_tg1_audit_ensures_retry_until_all_six_tables_are_keyed` (two refused attempts, then all six DEDUP keys accepted, each carrying `feed`), `test_regression_tg1_spill_drain_writes_nothing_while_the_dedup_key_is_refused`, `test_regression_tg1_spill_drain_keys_the_table_before_the_first_write`
  - Honest limit (unchanged, recorded at `DdlTables::AuditTables`): only the spill-replay path is gated on the key; a live consumer row written before the background re-run lands can still auto-create its table unkeyed.
- [x] TG-2 WAL catch-up against a stalling QuestDB (ack timeout, lag pause, clock run-out) (`chaos_catchup_slow_questdb.rs`)
  - Files: crates/app/tests/chaos_catchup_slow_questdb.rs (decision logic: `AppliedWatermark` ack accounting, `wal_catchup_lag_step`, `wal_catchup_budget_secs`; the pause-loop model is source-pinned to the loop in `dhan_feed_stack.rs`)
  - Tests: `test_regression_tg2_ack_wait_times_out_on_a_writer_stalled_by_questdb`, `test_regression_tg2_ack_wait_refuses_when_only_the_depth_writer_is_stalled`, `test_regression_tg2_ack_wait_succeeds_when_questdb_recovers_inside_the_bound`, `test_regression_tg2_ack_wait_refuses_a_batch_that_landed_nowhere_without_waiting_out_the_bound`, `test_regression_tg2_acks_during_a_questdb_stall_do_not_raise_the_skip_watermark`, `test_regression_tg2_lag_pause_against_a_stalled_questdb_ends_within_its_bounds_all_day`, `test_regression_tg2_lag_pause_resumes_on_the_first_poll_after_questdb_recovers`, `test_regression_tg2_pause_model_matches_the_catch_up_loop_source`
- [x] TG-3 archiver /exp 429 and slow export keep the partition and stop within a bound
  - Files: crates/storage/src/partition_archive.rs (one `mod` line), crates/storage/src/partition_archive/stub_integration_tests/tg_export_regressions_tests.rs (reuses the stub QuestDB + S3 harness)
  - Tests: `test_regression_tg3_export_429_keeps_every_partition_and_uploads_nothing` (429 body is a valid 3-row CSV; bite-checked: removing the `/exp` status check fails it), `test_regression_tg3_a_partition_kept_by_429_is_archived_once_exp_answers`, `test_regression_tg3_stalled_export_keeps_partition_and_pass_ends_within_bound`, `test_regression_tg3_trickling_export_keeps_partition_and_pass_ends_within_bound`
  - Bound tested here: per-run cap + one `/exp` per partition per pass + the export client timeout. The in-session time cap (`ArchivePassLimits`, PR DP #2196, merged after these tests were written) is covered by DP's own tests (`test_stub_with_pass_limits_skip_depth_cap_and_deadline`, `test_regression_deadline_only_pass_is_not_gated_on_wal_lag`, `test_pass_deadline_reached_boundaries`), not here.
- [x] TG-4 nightly contention test: production compose limits, one hour of depth, archiver export concurrent with ILP writes and /exec probes
  - Files: crates/storage/src/partition_archive.rs (one `mod` line), crates/storage/src/partition_archive/stub_integration_tests/tg4_export_contention_tests.rs (real QuestDB for `/exec`, `/exp` and ILP; the parent module's stub S3), crates/common/tests/ignored_test_guard.rs (allowlist entry), .github/workflows/chaos-nightly.yml (one step after the chaos step against the lane's compose stack, `TV_QDB_REAL_REQUIRED=1`; the build step folded to one line so the run-line budget stays 64; timeout 60 → 80 min), docs/error-runbooks/wave-2-error-codes.md (STORAGE-GAP-05 note)
  - Tests: `tg4_archiver_export_contention_against_real_questdb` (`#[ignore]`d): seeds one `market_depth` hour ten days back (`TV_TG4_DEPTH_ROWS`, default 5,000,000, about 5% of a live hour), then runs the real `archive_and_drop_old_partitions` while a `TickWriter` flushes `TV_TG4_TICK_ROWS_PER_SEC` (default 5,000) rows every 500 ms, `/exec` `SELECT 1` runs once a second and a second `/exp` stream of the same hour fills the export limit of 2. Fails when any flush or probe fails, a flush started in the pass takes over `ILP_REQUEST_TIMEOUT_SECS` (5 s), or a probe over `PROBE_REQUEST_TIMEOUT_SECS` (2 s); also fails unless the hour was exported and dropped, at least 5 flushes and 5 probes started inside the pass, and every tick row landed.
  - Not run against a real QuestDB in this session (no Docker daemon in the build container): compiled, and its skip path checked. The first scheduled chaos-lane run is its first real run; until then its pass/fail and its run time are Unknown. Concurrent writes go to `ticks`, not `market_depth` (a write to the exported table makes the pre-export WAL gate skip it).

### PR PF — 2026-11-06: per-tick in-memory path toward under 1 µs with live proof (storage, app, config)
**Approved by:** Parthiban, 2026-11-06 ("i need always less than one microsecodn across the entore codebase workspace woth a real tiem proof"; "go ahead fix everything dude approved"). Honest scope: in-memory per-tick steps only; network, disk and database steps cannot be under 1 µs.
- [x] PF-1 turn on `[tick_storage] own_ilp_encoder` in production config (byte-identical proof exists, bench p99 ~0.8 µs vs library 1.8–2.4 µs), with a one-session live read of `hot_path_latency_minute` per-tick figures before/after — `config/production.toml` `[tick_storage] own_ilp_encoder = true`, `production_config_wiring.rs::test_production_turns_on_the_own_ilp_encoder`; base.toml stays OFF. Live read: the before figures are the 2026-11-06 session (library encoder); the after read is the first session after deploy
- [ ] PF-2 measure core placement (QuestDB cpuset 0 vs main runtime workers) with `tv_hot_thread_run_delay_ns_total` and decide; no change without numbers
- [ ] PF-3 profile the drain's per-packet cost on the live shape (cold 25k-instrument maps) and cut the largest step
- PF measurement 2026-11-06 (x86, loaded host, 25,000 instruments, 1 packet/frame, DRAM-cold mode): production shape (Full, library encoder, 10 level rows) p50 7.5 µs / p99 22–30 µs per tick; with book rows 4.1 / 10.7 µs; plus own encoder 3.2 / 8.3 µs. Under 1 µs is not reachable while the drain encodes ILP text (~1.5 µs per Full tick alone). Ranked cuts, each with an old-vs-new proof:
- [x] PF-4 move ILP encoding off the drain: drain pushes a fixed ~200 B binary record per tick into a pre-allocated per-flush buffer; the writer thread encodes with the same write functions (byte-identical proptest over random tick streams; rescue/spill/watermark tests)
  - 2026-11-06 design panel (3 designs, 2 judges, read-only; `.claude/plans/research/pf4-design-2026-11-06.md`): corrections to this line: the staged record is 80 B (a copy of the persisted `ParsedTick` fields) or ~290 B (the existing `TickRow`), not ~200 B; the buffer is per batch and recycled; tick_quotes and feed_aux rows are included; three places encode (writer thread, rescue thread, the drain's cold inline fallback); depth book rows need a PF-4b of the same shape. Must-fix before code: per-batch contract-label pins (byte identity when labels republish mid-batch), encode-failure accounting on the writer before the ack, text bounds restated as row/RAM bounds plus a writer-side 32 MiB ceiling, golden fixtures recorded on today's code in an earlier commit. Owner decisions pending (see plan PF-4 open questions). Not measured: every ns figure is Assumed.
  - Owner decisions 2026-11-06 (decision card, verbatim labels): build it, **"Build it, existing row"** (the drain stages today's `TickRow` / `AuxPacketRow`, ~290 B; the text step moves to the writer and rescue threads; the 80 B record (PF-4c) is not built); encode failures **"Yes, count on writer (Recommended)"** (same drop counter, not-landed total and WAL replay; `IngestOutcome::WriteFailed` keeps `TickRowError` only); contract names **"Cap at 64 characters (Recommended)"** (longer names cut to 64 at publish with a counter, so the row size bound is a constant). Base design: the panel winner (risk-first, per-batch label pins).
  - Built 2026-11-06 (branch `pf4-on-main`; not deployed, not measured live). Files: `crates/storage/src/tick_stage.rs` (new: `StagedRow`, `StagedBatch`, pins, `encode_into` / `encode_counted`, the writer-side encode-failure backstop, `MAX_ILP_LINE_BYTES`, `MAX_PRODUCER_ROWS`, `TICK_STAGED_ROWS_PRESIZE`), `tick_persistence.rs` (drain stages; `pin_after_cut`; `FlushBatch` / `RescueBatch` carry staged rows; writer, rescue and cold paths encode; row bounds `PARKED_RESCUE_MAX_ROWS`; 32 MiB writer ceiling), `feed_aux_persistence.rs` (`append_aux_row_with_labels`), `candle_contract_labels.rs` (64-character cap, its own commit). Tests: golden fixtures recorded on the old code first (`tick_stage_golden_tests.rs`, `tests/fixtures/pf4_golden/`), pre-PF-4 append path kept verbatim as a proptest oracle (`tick_stage_oracle_tests.rs`: `proptest_pf4_staged_bytes_equal_the_legacy_append_{library,own}`, 1,024 cases each; pin-cut, writer-thread mock-server and rescue-thread tests), `tick_stage_tests.rs` (incl. `test_max_ilp_line_bytes_covers_a_maximal_row`), source-scan ratchet `tests/pf4_drain_never_encodes_guard.rs`, DHAT `tests/dhat_tick_row_append.rs` (zero per tick, also across a republish); every rescue/park/deferral/shutdown test ported, none deleted. Bench `tick_row_append` (x86 dev container, per row, median): full 1,111 → 123 ns (library) / 521 → 131 ns (own); own-encoder p99 1,165 → 332 ns. The text cost moved to the writer thread (not timed). Depth rows still encode on the drain (PF-4b).
- [x] PF-4b the same for the inline depth BOOK rows (`market_depth_book`, 2 per Full packet, live in production with `[depth_storage] array_rows` on): the drain stages a typed row, the depth writer thread, the depth rescue thread and the drain's cold `off_worker` paths write the text with the unchanged line writer
  - 2026-11-06 measured first (x86 dev container, release, Criterion median; same machine, same session; noisy host, the Full tail gate failed before and after): `depth_row_append/book_row_5` 402 → 63.3 ns per book row (p50 401 → 105 ns, p99 728 → 197 ns); `feed_drain/main_feed_full_frame_4_own` (4 Full packets, inline book rows, own tick encoder) 8.09 → 3.86 µs per frame (2.02 → 0.97 µs per tick); `feed_drain/main_feed_full_frame_4` (library encoder) 7.99 → 3.93 µs; new `feed_drain/main_feed_full_frame_4_own_nodepth` (same frames, no inline depth) 2.43 → 2.36 µs, so the depth share of the Full drain fell from 5.66 to 1.51 µs per frame (1.41 → 0.38 µs per tick). Full frame p99 (one `Instant` pair per frame): own 18.9 → 13.0 µs, library 18.5 → 10.5 µs; the amortized Full tail gate (2.5 µs per tick) still fails on this host (own 3.25, library 2.63 µs per tick, from 4.74 / 4.62). Not measured on the production host; not measured live.
  - Built 2026-11-06 (branch `pf4b-on-main`, local commits only; not pushed, not deployed). Files: `crates/storage/src/depth_stage.rs` (new: `StagedBookRow`, `DepthStagedBatch` with a value arena, pins, `book_line_len` / `escaped_symbol_len` (exact text size), `encode_into` / `encode_counted`, the writer-side encode-failure backstop), `crates/storage/src/depth_persistence.rs` (`write_book_line_into` moved out unchanged; `append_book_row_inner` stages; `stage_pin_after_cut`; run-table carry across a direct discard; `DepthFlushBatch` / `DepthRescueBatch` carry staged rows; writer-thread `encode_staged` into a reused scratch; rescue `spill_payloads` / `staged_text` / `recycle`; `park_bytes` by capacity; `spill_batch_on_drain`; spare staged lane), `crates/storage/src/lib.rs`, `crates/app/src/dhan_feed_stack.rs` (rescue thread recycles; `pending_book_bytes` returns the staged rows' text), `crates/app/benches/feed_drain.rs` (`_nodepth` target), `crates/storage/benches/depth_row_append.rs` (doc), CLAUDE.md (new `depth_stage.rs` row plus dated notes on the park, inline-depth, DT-small and tick_stage rows).
  - Tests: golden fixtures recorded on the OLD code in the earlier commit (`depth_stage_golden_tests.rs`, `tests/fixtures/pf4b_golden/`: three light streams compared batch by batch, flush and rescue queues, one heavy-publish stream compared whole), the old append kept as an oracle (`depth_stage_oracle_tests.rs`: `proptest_pf4b_ordered_bytes_equal_the_legacy_append` and `proptest_pf4b_stress_lines_equal_the_legacy_append`, 256 cases each, plus park-at-shutdown, rescue-thread, writer-thread, mid-run republish across a discard and ninth-table cut tests), `depth_stage_tests.rs` (`proptest_book_line_len_is_exact`, 512 cases; encode failure counted once; pins; pre-size; `test_pf4b_drain_book_append_never_writes_ilp_text` source ratchet). Existing DHAT gates cover the drain (`dhat_depth_book_row_presize` 0 blocks, `dhat_live_ingest_seam`), so no new DHAT target.
  - Review round 1 (2026-11-06, 6 lenses, 2 refuters per finding), fixed in one new commit: (1) a book writer's grown text buffer is dropped on the writer thread, never parked in the buffer lane it does not read (`test_book_writer_thread_drops_a_grown_text_buffer_instead_of_parking_it`); (2) `MAX_DEPTH_STAGED_SPARE_RAM_BYTES` derived from the 9 MiB text bound (`MIN_BOOK_LINE_BYTES` 128), about 29 MiB of address space per batch at the worst, so a batch grown only up to the flush trigger (depth-20/200 rows) is still recycled; the depth-200 arena growth is NEW on the drain vs the old text buffer, wording corrected (`test_a_batch_grown_to_the_flush_trigger_is_still_recycled`, `test_min_book_line_bytes_is_a_floor_for_every_label`); (3)/(5) the book regrow and crossing-frame tests now assert on the staged batch (capacities and pointers); (4) the v2 one-record spill test spills the real staged bytes; (6) book twin of the spare-boundary tests (`test_book_hand_off_installs_a_staged_spare_at_the_presize_boundary`); (a) new DHAT `crates/storage/tests/dhat_depth_book_hand_off.rs` (0 blocks for flush and rescue cycles with recycled batches and a mid-run name republish), listed in ci.yml (storage DHAT 12 -> 13); (b) the stress proptest no longer claims parks or inline spills; deterministic `test_inline_spill_of_staged_rows_writes_the_legacy_bytes` added beside the park test. The `_own_nodepth` bench id is gated by the existing `feed_drain_main_feed_full` budget and `feed_drain_main_feed_full_frame_4` divisor (substring; no new key).
- [ ] PF-5 batch the seal hand-off per frame (same `BufferedSeal` sequence, escalation path tested)
  - 2026-11-06 measured, NOT built (no change without numbers; the numbers argue against it). Harness `crates/storage/tests/seal_handoff_burst_harness.rs::seal_handoff_burst_cost_per_seal_vs_batched` (`#[ignore]`d, release, x86 dev container, load ~2.3, 200,000 bursts per shape, each figure includes one ~29 ns `Instant` pair divided by the burst size). Production shape (writer empties the queue every 100 ms, one burst per 5 µs frame), per-seal `try_send` vs a batched prototype (one crossbeam item holding up to 8 seals), ns per seal:

    | Seals per frame | per-seal p50 / p99 | batched p50 / p99 |
    |---|---|---|
    | 1 | 50 / 227 | 96 / 564 |
    | 2 | 38.5 / 260 | 67.5 / 314 |
    | 4 | 29.2 / 171 | 28.5 / 154 |
    | 8 | 34.5 / 130 | 17.5 / 79 |
    | 16 | 36.2 / 76 | 21.9 / 53 |
    | 64 | 15.3 / 64 | 8.3 / 41 |

    Batching is slower for 1 to 2 seals per frame (Assumed the common frame: one tick sealing its 1 s bar, plus 1 m at a boundary) and saves 10 to 20 ns p50 per seal only at 8 or more; every shape is already well under 1 µs per seal. A fixed-size batch item (the queue must not allocate) also costs 8× capacity at the same 46 MB (about 31,250 items, a 1-seal frame filling an 8-seal slot) or about 356 MB to keep 250,000 slots, and changes the hand-off type for every producer and the writer's take loop. Batching wins clearly only against a consumer spinning on `try_recv` (p50 165 → 17 ns at 64 seals), which production does not run. Recorded in the CLAUDE.md `SealSender` row. Reopen only if a live per-frame seal-count histogram shows bursts of 8 or more are common.
- [ ] PF-6 one shared instrument index (gap detector, backup dedup, spot store keyed by the aggregator slot; identical verdicts under proptest)
  - 2026-11-06 measured, NOT built (no change without numbers; the numbers show no gain). What the drain probes per tick today: (1) backup dedup: one bit test in an 8 KiB filter for every tick outside the backup set, a hash probe only for the backup set (≤ 1,000 by default) and filter false positives; it runs BEFORE the fold to drop second copies, so there is no fold slot to share and its common case is cheaper than any slot lookup. (2) spot price store: spot segments only (~4% of ticks), and the attach tasks read it by composite key from their own tasks, so its concurrent index has to stay. (3) gap detector: one composite-key probe of its own index on every tick, the only candidate.
  - Detector alone (x86 dev container, release, random keys over 25,000 per table, every figure includes one ~28 ns `Instant` pair): one table: `observe` p50 110–113 / p99 293–312 ns, its index probe alone p50 54 / p99 90, the update given the slot p50 75 / p99 182–193; 160 tables (~528 MB, out of cache): `observe` p50 648–663 / p99 1,118–1,148, probe alone p50 287–304 / p99 635–662, update given the slot p50 321 / p99 644–717. In isolation the probe is about half the detector's cost.
  - Prototype (not committed): `TickGapDetector::observe_hinted` / `observe_tracked_hinted` take a slot hint only when that slot is in use and holds the key, which is exactly when the index maps the key to it (proptest against `observe` with kept, stale, wrong and out-of-range hints, resets and refusals: 70 detector tests passed); the drain probes the fold's index first (`MultiTfAggregator::lookup`) and keeps one detector slot per fold slot (100 KB). End-to-end harness, detector + `consume_tick` per tick, in-bucket shape, alternating passes, 3 runs × 4 rounds: cold (6 tables × 25,000 per shape, ~0.5 GB) today p50 920–1,006 / p99 1,518–2,202 ns, prototype p50 906–991 (one outlier round 1,366) / p99 1,508–2,523 (outlier 4,743); warm (1 table each) today p50 515–613 / p99 1,118–1,421, prototype p50 535–682 / p99 1,172–1,305. No measurable gain in either shape; warm leans slightly worse.
  - Why (Assumed, consistent with the numbers): today the detector's probe and the fold's probe are independent loads, so the CPU overlaps their cache misses; the prototype makes the detector's state wait on the fold's probe and the hint read (one dependent chain), so the miss it removes was already hidden behind the fold's. Not measured on the r8g host (Assumed the same; its cores also overlap independent misses). Reopen only if a live profile (PF-3) shows the detector probe on the critical path.
- [x] PF-7 candle slot layout split (per-tick vs seal-only fields) + prefetch after the index lookup (restart_differential + consume_tick equality proptest) — shipped as the cache warm-up only; the layout split was measured and NOT built (note below)
  - Files: crates/trading/src/candles/fold_warm_up.rs (new), crates/trading/src/candles/aggregator_cell.rs (`AggregatorCell::warm_for_tick`), crates/trading/src/candles/multi_tf_aggregator.rs (call after `slot_index`, `#[path]` test module), crates/trading/src/candles/multi_tf_aggregator_pf7_tests.rs (new), crates/trading/benches/candle_fold_cold.rs (new), quality/benchmark-budgets.toml (`candles_cold_consume_tick`)
  - Tests: `proptest_warm_up_never_changes_a_bar` (256 cases; old fold = warm-up off on the thread; compares every emitted bar, every outcome and the whole slot state; bite-checked: a one-field write on the old path fails it at step 4), `test_warm_for_tick_runs_on_a_rolling_stream_and_matches_the_old_fold`, `fold_warm_up::tests::*` (incl. the compile-time line-coverage check `covers_every_line`); restart_differential, fold_golden_digest, dhat_multi_tf_fold unchanged and passing
  - 2026-11-06 measurement (x86 dev container, `candle_fold_cold`, twelve 25,000-slot tables walked at random, interleaved A/B, 4 rounds, per-tick Instant pair included): rolling shape p50 1.28–1.31 → 0.98–0.99 µs, p99 2.0–2.5 → 1.5–1.7 µs; in-bucket shape p50 1.13–1.18 → 0.89–0.94 µs, p99 2.0–3.8 → 1.4–1.6 µs; fully cache-hot `consume_tick_hit_500` +~25 ns per tick. Why no prefetch instruction: the trading crate denies `unsafe`, `_mm_prefetch` needs an `unsafe` block, and the aarch64 intrinsic is unstable in 1.95; plain loads run on both targets. Why no layout split: an `unsafe` whole-slot `prefetcht0` experiment (never committed), which is the most a hot/cold split can buy, measured cold-roll p50 0.91–1.05 µs, within the noise of the loads; the slot stays 2,864 B. Under 1 µs is still not met at p99 with the slot out of cache. Not measured on the r8g host (Assumed: a 36 MB L3 against 72 MB of slots behaves like the cold shape).
  - Review round 1 (2026-11-06): the bench now checks the shape it times (`ShapeCount::check` on the untimed settling pass, on the Criterion ticks and on every report pass: no refusal, no seal for Hit, a seal on more than half the ticks for Roll; `shape_check_self_test` proves it fails a Roll run labelled Hit, a Hit run labelled Roll and a refusal). Header states that the indexes (~1 MB per table) stay in the dev L3 so the slot is what misses, and that every figure includes one `Instant` pair (measured p50 29–39 ns, p99 39–48 ns over two runs). Criterion medians for the gate ids (one run each, not interleaved): `candles_cold/consume_tick_hit_25k` 1,011 ns and `consume_tick_roll_25k` 1,124 ns with the warm-up, 1,309 / 1,424 ns without it. `bench-gate.sh` reads Criterion's median, the median of the 30 per-sample MEANS, not the per-tick p50 above, so it reads higher than the report.
- [ ] PF-8 per-frame batching of per-tick atomic counters (ingest_ticks, conservation ledger, tick_ts_placement, tick_quotes count, repeat_quote)
  - Measured 2026-11-06, NOT shipped (no gain on the measured live shape). x86 dev container, release bench profile, production metrics recorder installed (`PrometheusBuilder::build_recorder`), `feed_drain` Full fixture with the own encoder and inline depth book rows; a temporary harness, not committed. The per-tick counters on the drain path are five: `tv_dhan_feed_ingest_ticks_total`, the conservation ledger (`note_row_appended`: load + `fetch_add`), the placement mark and counter (`fetch_max` + add), the `tick_quotes` counter, and one delivery-lag exclusion counter (repeat / not-new-trade / not-a-trade). Micro, the five per tick with a recorder: 4-tick frame p50 361 ns (about 80 ns per tick after the clock pair), one add per frame batched 81 ns. Drain, per-tick p50 (12 unbatched runs, 6 per batched build, interleaved): 4-packet frames 1,637-1,698 ns unbatched (median 1,648), 1,607-1,630 ns batched (median 1,614) and 1,615-1,650 ns with the batch only on stacked frames (median 1,619): about 30 ns per tick, 2%. 1-packet frames 1,665-1,761 ns unbatched (median 1,675), 1,690-1,769 ns batched (median 1,699) and 1,697-1,709 ns with the batch skipped for one-packet frames (median 1,703, one 2,331 outlier left out): about 25 ns slower in both builds, so the cause there is not the batch bookkeeping (Assumed: code layout between builds; not isolated). Without a recorder the drain read 1,581-1,640 ns, so every recorder-backed counter on the drain together is about 55 ns per tick. Built and tested before the decision: storage `tick_frame_counts` (`FrameCounts` held per frame, published on every way out; proptest against the row-by-row oracle, 1,024 cases, with ledger day-slot resets and the placement mark), app `begin_frame_counts` / `end_frame_counts` around the frame walk with the lag exclusions in `FrameTally`. Storage subset tests passed (30); the app tests were written but not run. Not shipped because the PF measurement's production shape is one packet per frame (live frames-per-tick ratio Unknown: `hot_path_latency_minute` `drain_main_frame` vs `drain_main_tick` samples would give it). Revisit only if that ratio shows stacked frames are common; the gain cap is about 55 ns per tick.

### PR AA — 2026-11-07: the box copies to S3 and deletes by itself (storage, app, config)

**Approved by:** Parthiban, 2026-11-07 (Quote 51: cards "Copy 6 Nov to S3, then delete" and "Approve both (Recommended)"; "ensure to cover al these cases before copying and deleting").

**Verified root cause (CloudWatch 3–7 Nov, S3 listing, code read):** the archiver dropped 201 partitions with verified copies but (1) deletes by age only (~24.5 h; `MIN_HOT_DAYS = 2` under pressure; hour path for `market_depth` only), (2) skips a whole table when `writerTxn != sequencerTxn` (233 skips, 15 pressure episodes freeing 0 bytes at 75–93%), (3) the daily leg latches done at ~15:47 while hours become eligible later, (4) runs ~50 min before the 16:30 stop and the drain hold does not count copy work, (5) a non-empty spill folder turns the `market_depth` hour path off for the daily leg.

#### Design
- AA-1 snapshot WAL gate: for a table with an old partition to copy, read `sequencerTxn` S, wait (bounded) until `writerTxn >= S`, while a process-wide archive quiesce lock holds every writer that can post into an old hour (WAL catch-up replay, spill replay, after-close depth pass); export, recount, verify, audit, re-check recount, drop. Live writers only post into the current hour.
- AA-2 hour path for `ticks`, `tick_quotes`, `market_depth_book`, `feed_aux_packets` (each table's `ts` meaning checked; IST correction as `market_depth`), 4 h floor, oldest hour first, never an hour with deferred-depth marks; the spill-folder switch-off of the hour path removed where the quiesce lock covers the spill replay.
- AA-3 projected-fill trigger: pressure also starts when hours-to-full (`DiskFillWindow`) is under hours-to-stop + 1; in session one depth partition per pass allowed then.
- AA-4 daily leg stays open until no eligible partition remains, re-runs every poll after the close until free space reaches max(1.5 × p95 session bytes, 30% of disk).
- AA-5 drain hold counts bytes still waiting to be copied; never past 07:30 IST.
- AA-6 local `/metrics` counters only; no new page.

#### Edge Cases
WAL never catches up inside the wait (skip, counted); S3 down or checksum mismatch (keep, counted); crash between upload and drop, between audit and drop, mid-export; replay writing an old hour during the copy (quiesce lock; recount mismatch keeps the partition and deletes this run's upload only); partition re-created after drop (copied again to a sidecar key); open (current) partition never selected; IST/UTC boundary at midnight; holiday, weekend, Muhurat; 6 Nov-sized day (10× volume); export over 600 s; upload over 5 GB; disk at 98% with the gzip temp file on the same disk; QuestDB restart mid-pass; deploy mid-pass; two passes racing (daily and pressure).

#### Failure Modes
Every failure keeps the partition (no drop without verified copy); the disk can still fill if S3 is down longer than the headroom or one session writes more than the free space (STORAGE-GAP-05 pages as today).

#### Test Plan
Unit tests for each gate; randomized 30-day simulation (volumes 0.5–10×, WAL lag, S3/verify failures, restarts, replays, crashes at every step) checking: no drop without verified copy, no drop of unapplied rows, SEBI/audit/candles untouched, deferred-depth hours kept, disk below the line except the stated cases; bite tests showing the old exact-equality gate fails the disk check; scoped `cargo test -p tickvault-storage -p tickvault-app`.

#### Rollback
Revert the PR; config switches restore the old windows.

#### Observability
`tv_partition_archive_wal_snapshot_wait_secs`, `tv_archive_backlog_bytes`, `tv_partition_archive_quiesce_held_total` (local `/metrics` only, no page).

- [ ] AA-1 snapshot WAL gate + quiesce lock
- [ ] AA-2 hour path for the four tables
- [ ] AA-3 projected-fill trigger
- [ ] AA-4 daily leg until done
- [ ] AA-5 drain hold counts copy backlog
- [ ] AA-6 metrics
- [ ] AA-7 randomized proof + bite tests
