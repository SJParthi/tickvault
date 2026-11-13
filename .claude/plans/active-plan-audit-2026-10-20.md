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

- [x] AA-1a quiesce lock + config keys (step 1): `crates/storage/src/archive_quiesce.rs` (`ArchiveQuiesce`, `hold_writers_for_archive`, `archive_quiesce_shared`, `try_archive_quiesce_shared`, `archive_quiesce_shared_blocking`, `archive_quiesce_for_replay`); writers wired: boot refold and catch-up chunks (`dhan_feed_stack.rs`, stop reason `archive_quiesce`), `replay_spill_dir`, `replay_framed_spill_dir`, the after-close depth pass per bucket; the eleven `[partition_retention]` keys with their defaults (`config.rs`, `base.toml`). Tests: `test_quiesce_writer_waits_behind_queued_archiver`, `test_archiver_write_times_out_against_held_reader`, `test_boot_refold_in_session_never_waits_on_quiesce`, `test_catchup_stops_with_archive_quiesce_reason`, `test_spill_round_ends_when_quiesce_busy_files_kept`, `test_framed_spill_round_ends_when_quiesce_busy_files_kept`, `test_partition_retention_pr_aa_keys_default_and_parse`. Nothing takes the archiver side yet, so no writer waits in production until step 2.
- [x] AA-1b snapshot WAL gate (step 2; the archiver takes `hold_writers_for_archive`): `crates/storage/src/partition_archive/snapshot_gate.rs` (`archive_with_snapshot_gate`, `wait_for_snapshot`, `wal_snapshot_verdict`, `snapshot_target`, `hour_frame_seq_span`, `hour_in_flight_verdict`, `process_in_flight_verdict`, `trait ArchiveIo`, `recheck_and_drop`); `PartitionArchiver` implements `ArchiveIo` (`copy_and_verify_partition`, `drop_copied_partition` extracted from `process_one`, which keeps the exact gate for every other table); `wal_applied_watermark.rs` (`rescue_floor_lowest`, `is_bound`); the DA-13d all-night `DEPTH_REWRITE_GATE` deleted, replaced by the per-hour after-close mark check (`deferred_depth_pass.rs`); `wal_lagging_tables` never lists a depth table; `ArchiveRunSummary` gained `held_quiesce_busy` (daily leg: `quiesce_busy`, refunded; pressure leg: stand-aside) and `held_in_flight` (neither); `disk_pressure_boot::depth_marks_hold_busy`. Snapshot-gate tables in step 2: `market_depth`, `market_depth_book` (both receipt-stamped; the depth pass writes both). Tests: `test_wal_snapshot_verdict_truth_table`, `test_hour_frame_seq_span_maps_ist_hours_to_utc_frame_sequences`, `test_in_flight_holds_an_hour_with_after_close_depth_marks` (+ one per reason), `test_s_read_before_lock_is_rejected`, `test_non_hour_tables_keep_exact_gate`, `test_pre_drop_recount_mismatch_keeps_and_deletes_own_upload`, `test_regression_da13d_depth_hour_with_after_close_marks_is_held_until_cleared`, `test_stub_depth_hour_archived_while_the_live_writer_keeps_committing`, `test_regression_da13d_marks_stay_set_through_the_settle_and_no_all_night_hold`, `test_quiesce_busy_refunds_and_in_flight_does_not_hold_the_day`, `test_quiesce_busy_stands_aside_but_in_flight_holds_escalate`, `test_depth_marks_hold_busy_only_after_close_with_marks`.
- [x] AA-2 hour path for the four tables (step 3): `crates/storage/src/partition_archive/hour_path.rs` (`HOUR_PATH_TABLES`, a closed list of the five Quote 51 tables checked at compile time against candles, SEBI, audit and retention-exempt names; `PRESSURE_HOUR_TABLES` = `market_depth_book`, `tick_quotes`, `feed_aux_packets`, on the hour path only while the pass carries pressure and `pressure_hour_tables_enabled`; `ticks` only under pressure and `ticks_hour_path_enabled`, which ships `false`; `hour_window_for`, `hot_hour_floor` (0 = off, floor 4 h), `may_force_with_spill_pending` (`market_depth` only, behind `market_depth_force_with_spill_pending`), `uses_snapshot_gate`); `partition_manager::build_closed_hour_list_sql` replaces the start-based hour builder and judges an hour by its END (`minTimestamp < now_ist − (N+1) h`, so `market_depth` keeps one more hour); `spill_dirs_for_table` is per table (depth folder for the depth tables, tick folder for `ticks`, `tick_quotes`, `feed_aux_packets`); `hour_window_decision` takes `may_force` (a pending spill defers every other table); the run lists each table through `PartitionArchiver::hour_window`, every table listed by the hour goes through the WAL snapshot gate and is never taken out of a gated pass for a lagging WAL. Owner decision 2 stays open: `#[ignore]`d probe `crates/storage/tests/questdb_dropped_partition_probe.rs` (`dropped_wal_partition_recreated_on_insert`, needs a scratch QuestDB, not run here). Tests: `test_hour_window_never_candles_sebi_audit`, `test_hour_path_list_check_bites_on_a_bad_name`, `test_hour_floor_end_based_at_least_4h`, `test_ticks_off_by_default`, `test_pressure_tables_only_under_pressure_and_switch`, `test_new_tables_not_forced_with_own_spill_pending`, `test_own_folder_only`, `test_snapshot_gate_follows_the_hour_path`, `test_spill_dirs_for_table_is_own_folder_for_hour_tables`, the 16-row `hour_window_decision_truth_table_is_exhaustive`.
- [x] AA-3 projected-fill trigger (step 4): `crates/storage/src/disk_health_watcher.rs` publishes its hours-to-full projection once a minute into one packed `AtomicU64` (Unix second + hundredths of an hour, so a reader never pairs a new figure with an old time); `published_hours_left` reads it only when fresh (at most `PROJECTION_MAX_AGE_SECS` = 300 s old, never stamped ahead of the clock), else unknown, and unknown is never short. `crates/storage/src/disk_pressure.rs`: `hours_to_stop` (07:30 IST before 07:30; on a session day the later of 16:30 IST and the gate end; otherwise the next 07:30, the drain hold's cap), `projected_short` (projection < hours-to-stop + `PROJECTED_FILL_MARGIN_HOURS` = 1 h), `PressureProbe::with_projected_short`; `decide_pressure_action` starts an episode when the projection is short and used is at least `pressure_projected_min_used_pct` (cooldown unchanged) and keeps it open below low water while still short; `EpisodeState::projected_only` (kept by `note_episode_level`, called after the decision and after the fold) means an episode the projection opened below high water never escalates: stuck below low water or projected-only, it ends quietly (no new STORAGE-GAP-05 firing) and the cooldown spaces the retry; `PressureGateReason::InSessionProjected` (in `ALL`, so registered at 0) and `projected_session_limits`: in session below 90% a short projection allows one depth partition a pass. `pressure_projected_fill_enabled = false` gives today's decisions at every used percentage (tested). Wired in `crates/app/src/disk_pressure_boot.rs` (`PROJECTION_STOP_CEILING_SECS_IST` from `DRAIN_HOLD_CEILING_SECS_IST`, stop from `SCHEDULED_DAILY_STOP_SECS_OF_DAY_IST`; the stood-aside rerun key is now "depth allowed"). Honest limits: the projection is the watcher's last-hour slope (an archive drop reads "not filling" until it leaves the window; the opening burst can over-read, which is why a projected-only episode never pages); the Muhurat stop is approximated as the gate end. Tests: `test_projected_short_around_1630_and_0730`, `test_stale_projection_is_unknown`, `test_published_hours_left_round_trips_through_the_atomic`, `test_projected_episode_not_ended_first_poll`, `test_projected_start_needs_min_used_pct_and_the_switch`, `test_projected_only_episode_ends_quietly_never_escalates`, `test_note_episode_level_marks_and_clears_projected_only`, `test_projection_off_keeps_todays_decisions`, `test_in_session_projected_allows_one_depth_partition`, `test_in_session_projected_registered_at_zero`, `test_projected_fill_is_wired_into_the_loop`.
- [x] AA-4 daily leg until done (step 5): `crates/app/src/daily_archive_boot.rs` (`archive_session_day`: a clock before 07:30 IST (`ARCHIVE_WINDOW_END_SECS_IST`, the drain hold's cap) belongs to the previous session day, so the loop no longer stops at midnight; `daily_latch_decision`: latches only on a clean pass that dropped nothing with free space known and at or above the target, a pass that dropped something or a clean pass below the target (or with free space unknown) gives its attempt back, a pass that fails without progress still spends one, so the six-attempt cap and the 07:30 end bound the loop; `should_measure_session`: the session day's size is read once after 15:40, the same calendar day, before this process drops any of its partitions; `daily_pass_config`: below the target the daily pass carries the hour-path marker (owner decision 3) but keeps every day window and never forces `market_depth` past a pending spill file; the loop probes the data volume before and after each pass, logs one "daily archive pass judged" line with free bytes and target); `crates/app/src/archive_free_target.rs` (`SessionBytesRing`, last 20 session days in `data/state/archive_session_bytes.json`, temp file + fsync + rename + folder fsync, a malformed file refused whole; `free_target_bytes` = max(`daily_free_target_session_multiplier_pct` × p95, `daily_free_target_pct` × disk), fallback session 40 GiB under 5 measured days, saturating; a 0-byte day is not recorded); `crates/storage`: `PartitionArchiver::session_bytes_on` (sum of `diskSize` over the day's partitions of the five Quote 51 tables, absent table 0, any unreadable table an error), `partition_manager::{build_partition_size_sql, parse_partition_sizes, sum_partition_bytes_on_day}`; `spawn_supervised_daily_archive_loop` takes the data dir (`main.rs` passes `data`, the pressure loop's path). Honest limits: hours the pressure leg dropped during the session are not in that day's size (a pressure day reads low); the target is not capped at the disk size (an unreachable target only keeps the leg polling until 07:30); no new metric (AA-6). Also raised `source_scan_budget_guard` `disk_pressure_boot.rs` 7 → 8 for step 4's `test_projected_fill_is_wired_into_the_loop`, which had left that guard red. Tests: `test_loop_continues_past_midnight_until_0730`, `test_no_latch_while_eligible_or_below_target`, `test_drop_refunds_attempt_and_attempts_cap_bounds`, `test_should_measure_session_once_after_1540_by_a_process_that_saw_it`, `test_daily_pass_config_below_target_uses_hour_path_only`, `test_free_target_p95_and_empty_ring_fallback`, `test_record_replaces_the_same_day_and_keeps_the_newest_twenty`, `test_ring_file_atomic_write_and_corrupt_refused`, `test_disk_size_column_parsed`, `test_parse_partition_sizes_refuses_partial_answers`, `test_sum_partition_bytes_on_day_matches_day_and_hour_names_only`, `test_build_partition_size_sql_lists_name_and_disk_size`, `test_stub_session_bytes_on_sums_the_days_partitions_of_the_five_tables`.
- [x] AA-5 drain hold counts copy backlog (step 6): `crates/storage/src/partition_archive.rs` (`ArchiveRunSummary::archive_backlog_bytes`: the bytes, by `diskSize`, of the worklist after the cross-verification hold, less the partitions the pass dropped and those it held in flight or for after-close depth marks; measured only with `PartitionArchiver::with_backlog_measure`, one size listing per table with work, before any cap, so an unreached partition still counts; `None` if any listing fails; `add_worklist_sizes`, `backlog_bytes_left`; `session_bytes_on` now shares `partition_sizes`); `crates/app/src/archive_backlog.rs` (`ARCHIVE_BACKLOG` cell, `backlog_holds`: holds only while the reading is fresh (under 15 min, or a daily pass running that started under 2 h ago), above 0 and measured with free space below the target; `held_backlog_bytes`); `crates/app/src/daily_archive_boot.rs` (the daily leg measures when `drain_hold_counts_archive`, notes the pass start, publishes after a pass that ran, keeps the old reading ageing after one that did not); `crates/app/src/drain_hold.rs` (`DayWork::archive_backlog_bytes`: in `is_done`, in the parameter text as `archive_bytes=`, and the 6th `ProgressTracker` anchor, so a backlog that stops falling releases after the hour's stall rule; never past 07:30). Honest limits: a pass that runs longer than 2 h without returning stops holding; the reading is published per pass (5 min apart), not live; nothing counts while `drain_hold_counts_archive = false`. Tests: `test_hold_released_when_backlog_stale_or_zero_or_target_met`, `test_running_pass_keeps_the_reading_fresh_but_bounded`, `test_publish_note_and_reading_round_trip`, `test_held_backlog_bytes_reads_the_process_cell`, `test_archive_backlog_holds_and_its_fall_is_progress`, `test_never_past_0730`, `test_add_worklist_sizes_keeps_only_worklist_partitions_of_the_table`, `test_backlog_bytes_left_skips_settled_and_saturates`, `test_stub_pass_measures_the_backlog_it_leaves`.
- [x] AA-6 metrics (step 6): `tv_archive_backlog_bytes` (`archive_backlog::ARCHIVE_BACKLOG_GAUGE`, set per published pass) and `tv_partition_archive_wal_snapshot_wait_secs` (`partition_archive::WAL_SNAPSHOT_WAIT_GAUGE`, the last snapshot wait in seconds, set in `wait_for_snapshot`), both registered at 0 when the daily loop starts; `tv_partition_archive_quiesce_held_total{writer}` was step 1. Local `/metrics` only: no page, alarm, metric filter or EMF name. Test: `test_register_archive_snapshot_wait_gauge_names_the_series`.
- [x] AA-7 randomized proof + bite tests (step 6): `crates/storage/src/partition_archive/archive_lifecycle_sim_tests.rs` (test-only). Runs the REAL `archive_with_snapshot_gate`, `recheck_and_drop`, `hour_in_flight_verdict` (over a real `DeferredDepth` mark table, overflow included), `hour_frame_seq_span`, `hour_window_for` / `hot_hour_floor` and the hour offset read from `build_closed_hour_list_sql`, against a modelled QuestDB WAL, S3 (sidecar key, never an overwrite), old-hour writers (spill replay on a later evening, after-close depth pass, post-stall rescue), sink acknowledgements and the worst-case drop (committed unapplied rows lost). 30 days × 256 cases (`proptest_archive_lifecycle_keeps_every_invariant`): day volumes 0.5–10×, WAL lag 0–5,000, S3 put and checksum failures and whole-night outages, crashes at a random call (after upload, before drop), sink stalls, spills, deferred marks with overflow, a 2 h backward clock step, holidays, weekends, Muhurat, in-session capped passes, flaky QuestDB probes, uploads over the size limit. Invariants: no drop without the copy, no row lost, no unapplied row dropped, no in-flight or marked hour dropped, the 4 h floor, candles and audit never dropped, and on a clean night every old hour gone except the stated cases. Bite tests (each finds a seed where the variant breaks its invariant and the real protocol on the same seed is clean): exact-equality gate (DISK), no quiesce lock (LOSS), S read before the lock (LOSS), no acked-watermark check (INFLIGHT_DROP), all-night depth hold (DISK), start-based hour SQL (FLOOR); `test_sim_exercises_drops_sidecars_and_holds` proves it drops and re-copies to sidecars. Honest limits: QuestDB, S3 and the writers are a model, not the real services; racing passes are not exercised (passes run one after another, as `ARCHIVE_PASS_LOCK` enforces); the real `place_archive` and `process_one` are not driven; the rescue floor and unapplied ranges inputs are fixed (empty) in the model.
- [x] PR AA-F (follow-ups to #2213: LOW findings 8, 9, 11, 12 and the query_activity check), 2026-11-09, under the PR AA approval above (Quote 51); findings 10, 17 and 19 untouched.
  - [x] AA-F8 the 30-day simulation never stamps a row after its own clock: the pass instant is drawn first, the hour's batches land before and after it, and a future stamp is a `SIM future_stamp` violation.
    - Files: crates/storage/src/partition_archive/archive_lifecycle_sim_tests.rs
    - Tests: proptest_archive_lifecycle_keeps_every_invariant, test_sim_exercises_drops_sidecars_and_holds, test_sim_drops_in_session_without_a_false_clock_ahead
  - [x] AA-F9 the process in-flight reader runs over borrowed state (`ProcessInFlightState`, `process_in_flight_verdict_on`), so a test drives each input; the wrapper is pinned to the process globals by a source scan.
    - Files: crates/storage/src/partition_archive/snapshot_gate.rs
    - Tests: test_process_in_flight_verdict_on_reads_each_process_input, test_process_in_flight_verdict_wrapper_passes_the_process_globals
  - [x] AA-F11 session bytes count hour-path partitions any leg already dropped that day (RAM tally; a still-listed recorded partition counts at the larger of its listed and recorded size; fail-closed, permanently for the process, on a gone partition of unknown size or more than 160 records in a day; the daily leg reads only a session its process saw from the 09:00 IST open and retries a failed listing after drops). Review of the fix (2026-11-09): the tally's size listing runs before the last-moment recount (5 s cap), never between the recount and the DROP; the pass-start size listing runs after the depth skip, the WAL-lag removal and the per-run cap.
    - Files: crates/storage/src/partition_archive.rs, crates/storage/src/partition_archive/drop_tally.rs, crates/storage/src/partition_archive/snapshot_gate.rs, crates/storage/src/partition_archive/archive_lifecycle_sim_tests.rs, crates/app/src/daily_archive_boot.rs
    - Tests: test_stub_session_bytes_on_counts_partitions_a_pressure_pass_dropped, test_session_bytes_on_refuses_a_day_with_an_unknown_dropped_size, test_hour_path_dropped_tally_keeps_four_days, test_session_bytes_with_drops_counts_a_still_listed_drop_once, test_session_bytes_with_drops_refuses_only_a_gone_unknown_partition, test_session_bytes_with_drops_keeps_the_dropped_bytes_of_a_recreated_hour, proptest_listing_plus_tally_is_the_day, test_regression_recount_is_the_last_query_before_the_drop, test_regression_unknown_dropped_size_is_a_permanent_refusal, test_regression_pass_start_size_listing_skips_tables_the_pass_cannot_drop, test_regression_process_saw_whole_session_only_when_armed_before_the_open, test_regression_refused_session_reading_is_left_out_and_unread_is_retried
  - [x] AA-F12 the drain-hold backlog leaves out a depth hour whose acknowledgement cannot arrive tonight (the highest ack any depth row can still carry, from the graced end of the day's arrival window, Muhurat-aware, reads NotAcked at the gate's own line; never with a recorded clock step).
    - Files: crates/storage/src/partition_archive/snapshot_gate.rs, crates/storage/src/partition_archive.rs, crates/app/src/archive_backlog.rs
    - Tests: test_last_depth_arrival_ist_and_depth_hour_cannot_clear_tonight_boundaries, proptest_cannot_clear_tonight_agrees_with_the_gate, test_retain_copyable_tonight_and_add_pending_hour_sizes_leave_out_a_depth_hour_that_cannot_clear_tonight, test_stub_busy_pass_leaves_out_a_depth_hour_that_cannot_clear_tonight
  - [x] AA-F13 query_activity column and CANCEL QUERY syntax checked against the QuestDB 9.3.5 source: matches, doc comment only.
    - Files: crates/storage/src/contract_name_repair.rs
    - Tests: none (doc comment)
- [x] PR AA-G (last #2213 leftovers: 10, 17, 19), 2026-11-09, under the PR AA approval above (Quote 51). No page, alarm, filter, EMF name, config, disk, IOPS or instance change.
  - [x] AA-G10 an accepted archive DROP is drain-hold progress: the backlog reading moves only at the END of a pass, so a pass longer than an hour that kept dropping was judged stalled (and one that rose, then fell above its old level, never counted); a running pass's reading also went stale 2 h after the pass STARTED. Now `partition_archive::archive_drops_total` (raised only on a DROP QuestDB accepted, never on a failed drop, a verify, an upload or a reused copy) feeds `ProgressTracker::observe_with_archive`, and `backlog_holds` keeps a running pass fresh while it dropped a partition in the last 2 h (`last_archive_drop_unix`, only a drop at or after the pass start). Still bounded: 07:30 IST window and expiry unchanged; no drop and nothing else moving for an hour still releases (a stuck backlog re-verifying one partition every pass); a running pass with no drop for 2 h goes stale.
    - Files: crates/storage/src/partition_archive.rs, crates/app/src/drain_hold.rs, crates/app/src/archive_backlog.rs, crates/app/tests/cov_drain_hold.rs, CLAUDE.md
    - Tests: test_archive_drops_total_and_last_archive_drop_unix_count_a_drop, test_archive_drops_total_is_raised_only_on_an_accepted_drop, test_stub_full_flow_verifies_audits_then_drops, test_regression_archive_drop_mid_pass_is_progress, test_regression_backlog_rise_then_drops_is_progress, test_observe_with_archive_stuck_backlog_without_drops_still_releases_after_an_hour, proptest_observe_with_archive_stalls_exactly_an_hour_after_the_last_drop, test_observe_with_archive_is_wired_to_archive_drops_total (crates/app/tests/cov_drain_hold.rs), test_regression_running_pass_that_keeps_dropping_stays_fresh_past_two_hours
  - [x] AA-G17 the after-close depth pass holds the archive quiesce lock for each FLUSH only (`flush_quiesced`), not for a whole bucket of segment reads, staging and flushes, so a queued archiver waits for at most one flush instead of a whole bucket (⚠ CORRECTED 2026-11-11, review: one flush can itself take about 87 s book / 169 s level at the library's minimum throughput, so the 120 s wait can still run out on a slow QuestDB; Risk, unmeasured). Safe: the pass writer is synchronous, so a flush is the only step that commits rows (⚠ CORRECTED 2026-11-11: plus the book writer's in-append pin-cut commit, now under the same lock, see the review fixes below); marked hours stay protected by the after-close marks (cleared only after the whole pass and the settle); unmarked hours get the same per-transaction exclusion as before.
    - Files: crates/app/src/dhan_feed_stack/deferred_depth_pass.rs, crates/app/src/dhan_feed_stack.rs, crates/storage/src/archive_quiesce.rs, CLAUDE.md
    - Tests: test_regression_item17_only_flushes_hold_the_archive_quiesce_lock, test_flush_quiesced_with_holds_the_lock_only_for_the_flush, test_flush_quiesced_with_returns_the_flush_error, test_regression_m4_floor_wait_runs_before_the_first_bucket, the_late_depth_paths_use_the_late_trigger_and_the_live_drain_does_not
  - [x] AA-G19 NOT A DEFECT, no code change: a `market_depth` hour archived while one of its spill files is pending (`market_depth_force_with_spill_pending = true`) loses no rows when the spill replays into the dropped hour. Verified in the QuestDB 9.3.5 source: DROP removes the partition from the transaction table (TableWriter.java:5471-5537), the O3/WAL apply starts an empty partition for a missing hour (TableWriter.java:7568-7584, version txn-1) and adds it back at commit (TxWriter.java:530-535), the read-only skip applies only to an existing partition (:7587), and the table has no TTL (depth_persistence.rs CREATE). Our archiver lists the re-created hour again and copies it to a content-addressed sidecar key, never overwriting or deleting the first copy (`place_archive`, `sidecar_archive_key`; tests below). Risk (Assumed): rows of an ambiguous flush can sit in both the main and the sidecar copy, so S3 readers de-duplicate on the DEDUP key. Unknown: the `#[ignore]`d probe `questdb_dropped_partition_probe.rs` has never run on a real QuestDB (it also writes through /exec, not ILP); re-check on any QuestDB upgrade.
    - Files: crates/storage/src/partition_archive.rs (read, unchanged: `place_archive`, `sidecar_archive_key`), crates/storage/tests/questdb_dropped_partition_probe.rs (read, unchanged)
    - Tests: a_sidecar_key_can_never_collide_with_the_canonical_key, the_conflict_arm_never_uploads_to_the_canonical_key, test_stub_f1_conflict_different_length_never_overwrites, test_sim_exercises_drops_sidecars_and_holds
  - [x] AA-G review fixes (2026-11-11, 15 findings confirmed by 2 of 3 refuters, merged into 8 fixes): (1) the book writer's in-append pin-cut commit (`stage_pin_after_cut`) now holds the archive quiesce lock too (`DepthWriter::hold_archive_quiesce_on_pin_cut`, set by `DepthIngest::new_book_only`), and the item-17 source scan checks more spellings of a lock take (⚠ CORRECTED 2026-11-11, review 2: "every spelling" overclaimed; it is a substring list, now including the point-free `::shared_blocking` and `archive_quiesce_for_replay`, and it cannot see a lock taken inside a helper the pass calls); (2) `flush_quiesced` skips the lock when nothing is pending and re-reads the disk floor under the lock (`QuiescedFlush::DiskLow`); (3) the wait is stated honestly (Risk, unmeasured: archiver up to about 87 s book / 169 s level per flush, so its 120 s can still run out on a slow QuestDB; the pass waits once per committing flush behind one archived partition) and the pass logs its total `quiesce_wait_ms`; (4) a drop counts as progress only while the archive backlog holds, and never re-anchors the other parts; (5) per-archiver `ArchiveDropLedger` so the stub test asserts an exact count; (6) the latest drop instant is stored, not max'ed, and a future stamp reads as now; (7) the vacuous start-filter case and the weak source pin are replaced with tests that bite; (8) the per-bucket docs in `partition_archive.rs`, `archive_quiesce.rs` and the CLAUDE.md snapshot_gate row carry dated notes. The helper test was renamed to what it proves (`test_flush_quiesced_with_holds_the_lock_only_for_the_flush`).
    - Files: crates/storage/src/depth_persistence.rs, crates/storage/src/depth_stage_oracle_tests.rs, crates/storage/src/partition_archive.rs, crates/storage/src/archive_quiesce.rs, crates/app/src/dhan_feed_stack.rs, crates/app/src/dhan_feed_stack/deferred_depth_pass.rs, crates/app/src/drain_hold.rs, crates/app/src/archive_backlog.rs, CLAUDE.md
    - Tests: test_hold_archive_quiesce_on_pin_cut_holds_the_in_append_commit, test_hold_archive_quiesce_on_pin_cut_uses_the_process_lock, test_regression_item17_only_flushes_hold_the_archive_quiesce_lock, test_flush_quiesced_with_holds_the_lock_only_for_the_flush, test_flush_quiesced_with_skips_the_lock_when_nothing_is_pending, test_flush_quiesced_with_reads_the_floor_under_the_lock, test_flush_quiesced_with_returns_the_flush_error, test_regression_drop_counts_only_while_the_archive_backlog_holds, test_regression_drop_does_not_reanchor_a_swinging_part, proptest_observe_with_archive_stalls_exactly_an_hour_after_the_last_drop, test_archive_drops_total_and_last_archive_drop_unix_count_a_drop, test_regression_drop_ledger_keeps_the_latest_drop_not_the_largest_stamp, test_archive_drops_total_is_raised_only_on_an_accepted_drop, test_stub_full_flow_verifies_audits_then_drops, test_regression_backlog_holds_reads_a_future_drop_stamp_as_now, test_regression_running_pass_that_keeps_dropping_stays_fresh_past_two_hours
  - [x] AA-G20 (2026-11-11, noise lock §2.17 note, owner card "Quiet on planned restarts (Recommended)"; Verified live 2026-11-09: every planned evening-drain restart at 3:54, 4:34 and 5:14 PM IST paged twice on the replay DEFERRAL lines; nothing is lost, deferred segments stay `*.wal`). On the planned evening-drain boot (`wal_replay_frame_cap::planned_evening_drain_boot`: the evening cap, or the claim recorded by `claim_planned_boot` through `note_planned_evening_drain_claim`, since STAGE-C runs before the cap is raised) both lines are coded `warn!`: `WS-SPILL-01` `replay_deferred_planned` (`report_catchup_replay_deferred`) and `WS-SPILL-02` `replay_budget_deferred_planned` (`report_replay_budget_deferred`); every other boot keeps the ERROR byte for byte; every counter kept; no page, alarm, filter or EMF name. ⚠ CORRECTED 2026-11-11 (review 2): "byte for byte" was false; same fields and text, but the log `target` moved to `tickvault_storage::wal_replay_frame_cap` (no filter keys on it). A WS-SPILL-02 MEMORY-guard stop now keeps the ERROR on the planned boot too (`test_regression_memory_guard_deferral_pages_even_on_the_planned_boot`). The claim arm is pinned in its own test process (`crates/storage/tests/planned_evening_drain_claim_process.rs`). Owner confirmed 2026-11-11 on the card "Yes, quiet them too (Recommended)".
    - Files: docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md, .claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md, crates/storage/src/wal_replay_frame_cap.rs, crates/storage/src/ws_frame_spill.rs, crates/app/src/dhan_feed_stack.rs, crates/app/src/evening_drain.rs, CLAUDE.md
    - Tests: test_regression_report_catchup_replay_deferred_warns_only_on_the_planned_boot, test_regression_report_replay_budget_deferred_warns_only_on_the_planned_boot, test_note_planned_evening_drain_claim_and_planned_evening_drain_boot, test_replay_budget_deferral_is_reported_with_the_planned_verdict, test_claim_planned_boot_records_the_verdict_for_the_replay_deferral_lines
- [x] AA review fixes (2026-11-08, ten confirmed findings): (1) the snapshot gate re-reads the table's own spill folder under the quiesce lock for every table listed by the hour and not forced (`InFlightVerdict::SpillPending`, `PartitionArchiver::spill_recheck_tables`); (2) the drain-hold backlog also counts hours that have ended but become old enough later tonight (`add_pending_hour_sizes`, cross-verification holds excluded); (3) a pass that only waited on a WAL QuestDB had not applied gives its attempt back, and `pass_verdict` names a list or partition failure before WAL lag; (4) the Quote 31 evening restart reads `DayWork::data_in_database`, not the archive copy work; (5) a replay registers `intend_old_hour_replay` for the boot refold and catch-up (the archiver starts no new partition meanwhile) and each catch-up chunk waits to the drain's own deadline (`archive_quiesce_for_replay_until`); (6) the pressure leg waits for depth marks only after a pass that stood aside solely for them (`stood_aside_only_for_depth_marks`); (7) the daily pass stops at 07:30 IST (`daily_pass_stop_unix_secs`); (8) the dropped-partition probe fails on accepted-and-lost (`probe_outcome_is_safe`); (9) the sim reports every table's WAL, bite `HourPathOnEveryTable` → EXEMPT_TABLE; (10) the sim's DISK horizon is anchored to the floor with a 7 h margin, bite `LateHourSql` → DISK. Tests: `test_regression_spill_file_written_after_the_listing_holds_the_hour`, `test_add_pending_hour_sizes_counts_hours_eligible_later_tonight`, `test_regression_wal_lag_passes_never_exhaust_the_night`, `test_regression_archive_backlog_never_blocks_the_evening_restart`, `test_regression_archiver_stands_aside_while_a_replay_is_registered`, `test_catchup_wait_runs_to_its_own_deadline_outside_the_window`, `test_catchup_chunk_outlasts_a_long_archive_partition`, `test_regression_quiesce_busy_stand_aside_never_waits_for_depth_marks`, `test_daily_pass_stops_at_the_window_end`, `test_probe_outcome_judge_fails_accepted_and_lost`, `test_bite_hour_path_on_every_table_drops_candles_or_audit`, `test_bite_late_hour_sql_fails_the_disk_invariant`. Honest limits: a pending hour that only becomes old enough after 07:30 still counts (the hold stops at 07:30 anyway); while a replay is registered the pressure leg stands aside too (bounded by the catch-up budget).

### PR RD — 2026-11-08: the WAL replay backlog drains more than one batch per boot (storage, app, docs)

**Approved by:** the 5x evening frame cap is Quote 41 (2026-10-27, card "5x for evenings (Recommended)", recorded in `daily-universe-scope-expansion-2026-05-27.md`), never built until now; the log and page change is the owner's 2026-11-08 decision card "Quiet on planned restarts (Recommended)", recorded first as §2.17 of `dhan-rest-only-noise-lock-2026-07-14.md`. The depth-deferral change (RD-3) reuses the existing item 45a deferred-depth mechanism and adds no page, table or setting.

**Verified root cause (CloudWatch `/tickvault/prod/app` 7–8 Nov 2026, code at `origin/main` 7205ad002):** (1) every boot replays ONE 512 MiB batch (about 3.5 million frames, 5 segments) and `WAL_REPLAY_MAX_FRAMES_PER_BOOT` (3,000,000, `crates/storage/src/ws_frame_spill.rs`) is checked only before a pass, so every catch-up round is refused at round 0 (`rounds 0, frames 0`); (2) `evening_drain::catchup_budget_for` (`crates/app/src/evening_drain.rs`) lengthens only the clock, so Quote 41's 15,000,000 is not in the code; (3) `wait_for_offload_drained` (`crates/storage/src/wal_applied_watermark.rs`) returns false the moment any row landed nowhere, and depth rows lost to a full depth spill (`crates/storage/src/depth_persistence.rs`) count, so the same 5 segments were re-offered on 7 Nov and at the 8 Nov Muhurat boot; the line said `replay_ack_wait_timeout`, `wait_secs 30`, although no wait ran (`crates/app/src/dhan_feed_stack.rs`). Backlog 298 → 290 segments over four boots; at 5 segments per planned restart, about 19 evenings.

#### Design
- RD-1 per-boot frame cap (storage): `EVENING_DRAIN_REPLAY_FRAME_CAP = 5 × WAL_REPLAY_MAX_FRAMES_PER_BOOT`; a process-wide atomic cap for this boot, default 3,000,000, raised once (never lowered, never past 15,000,000) by `raise_replay_frame_cap_for_planned_drain`; the replay pass takes the cap as an argument (pure, testable) and the production wrapper loads the atomic; the refusal line reports the cap used. New code goes in a new module so `ws_frame_spill.rs` and `main.rs` stay at or under their size ceilings.
- RD-2 evening allowance (app): `catchup_budget_for` returns seconds AND frame cap from one pure decision; `budget_or` stays one read-and-clear under the existing `STATE_IO` lock (one boot only); the catch-up caller raises the cap before its first round; the boot pass does not need it.
- RD-3 lost depth rows stop holding tick segments (storage, app): split the not-landed count into ticks and depth; a depth range lost after its spill also failed is marked in the deferred-depth table (`note_shed` per bucket, both depth kinds) and counted deferred only if no bucket was refused or overflowed; the ack wait returns `Landed`, `LandedDepthDeferred`, `TicksLost` or `TimedOut`; `LandedDepthDeferred` only when every depth loss was deferred and the marks were persisted, else hold as today. The segment can then be archived for its ticks while the deferred mark keeps it from being pruned until the after-close pass rewrites the depth rows.
- RD-4 log lines (§2.17): `replay_frame_cap` on a planned evening restart is a coded `warn!` `source = "replay_frame_cap_planned"` (counter kept); every other boot keeps the ERROR; a row lost after the ack is `error!` `source = "replay_rows_lost"`; a real timeout keeps `replay_ack_wait_timeout`; `LandedDepthDeferred` logs a coded `warn!` `source = "replay_depth_deferred"` with the row count.
- RD-5 docs: dated note at Quote 41 (the 5x cap is built, naming `EVENING_DRAIN_REPLAY_FRAME_CAP`); CLAUDE.md complexity-table row for the per-boot cap atomic and the depth-deferral arm, and a correction of the PR M row's "the per-boot cap still bounds each boot".

#### Edge Cases
Pending flag stale (older than the evening window) or state file unreadable (3,000,000, not planned); two processes in one evening (the flag is cleared by the first read); a boot pass that alone exceeds 15,000,000 frames (the boot pass reads one batch whatever the cap; catch-up refused as today); a planned restart inside 09:00–15:40 (outside the evening window: not planned); a deferred-depth table overflow or a bucket refused (counts as tick loss: hold); a marks persist that fails (hold); tick and depth rows both lost in one batch (hold); a depth range straddling a 2^38 bucket boundary (every bucket in the range marked); SIGTERM mid-replay (unchanged: the batch stays unconfirmed); disk under the replay floor (each pass still refused on its own).

#### Failure Modes
A wrong "planned" mark quiets one boot's cap line only (counter and `warn!` remain; next boot fails closed). If the depth marks are lost after archive (marks file rejected on boot), the after-close pass cannot find the rows: the marks file is persisted before `LandedDepthDeferred` is returned, and a rejected marks file reads as an unknown overflow, which keeps every segment (keep more). Below the 5% disk floor, pinned segments are still deleted last and each delete is a counted WS-SPILL-02 loss (unchanged limit). Each extra replay pass uses QuestDB disk; the replay disk floor still refuses each pass (whether the floor suits the 150 GB root is Unknown).

#### Test Plan
Pure tests for the allowance (15,000,000 only with a fresh pending flag inside the evening window; 3,000,000 otherwise) and for the cap (`frames_so_far` 3.5M refuses at 3M, reads at 15M); extend `budget_or_applies_to_one_boot_and_fails_closed_on_a_bad_file` so the second boot is back at 3,000,000; ack-outcome unit tests (tick loss → `TicksLost`, marked depth loss → `LandedDepthDeferred`, overflowed table → `TicksLost`, failed persist → `TicksLost`); update `unlanded_total_and_note_unlanded_fail_the_ack_wait…` for the split counts; an integration test replaying a batch with depth sends forced to fail (segment reaches `archive/`, `segment_is_deferred` true, the archive prune keeps it); bite tests (revert the wrapper to the constant: the 15M case fails; drop the `note_shed` call: the prune-keeps-it assertion fails); a test that the planned path logs `warn!` and the unplanned path `error!`; scoped `cargo test -p tickvault-storage -p tickvault-app`; `error_level_meta_guard`, `error_code_tag_guard`, `source_size_ratchet_guard`, `assertion_free_test_ratchet`, `source_scan_budget_guard`, banned-pattern scanner, pub-fn-test guard.

#### Rollback
Revert the PR: the cap returns to the constant, the ack returns to all-or-nothing and the lines to their old sources. Deferred-depth marks written by RD-3 are honoured by the old code too (item 45a), so nothing is lost on a revert.

#### Observability
`tv_wal_replay_frame_cap_stops_total` unchanged (counted on both paths); the cap used is in the refusal line and the catch-up summary; the depth deferral is visible on the existing deferred-depth gauges and the `replay_depth_deferred` `warn!`. Local `/metrics` and logs only: no new page, alarm, metric filter or EMF name (§2.17). Read after deploy: segments archived per planned restart (expected about 21 instead of 5) and `tv_wal_unapplied_pinned_segments` falling night over night.

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] RD-0 rule-file-first: §2.17 in `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md` and its stub bullet; this plan item.
- [x] RD-1 per-boot frame cap (storage): `crates/storage/src/wal_replay_frame_cap.rs` (`EVENING_DRAIN_REPLAY_FRAME_CAP`, `raise_replay_frame_cap_for_planned_drain`, `replay_frame_cap_this_boot`, `refuse_past_frame_cap`, the refusal line now names the cap used); `ws_frame_spill::replay_all_with_report_at` takes `frame_cap`, `replay_all_with_report_fenced` passes the atomic. Tests: `replay_refuses_past_the_per_boot_frame_cap` (3.5M: 3M refuses, 15M reads), `test_regression_fenced_replay_runs_under_the_raised_cap_not_the_constant`, `test_raise_cap_in_raises_once_never_lowers_never_passes_the_evening_cap`, `test_clamp_replay_frame_cap_holds_the_two_quote_41_bounds`, `test_refuse_past_frame_cap_uses_the_cap_it_is_given`, `test_raise_replay_frame_cap_for_planned_drain_keeps_replay_frame_cap_this_boot_in_bounds`.
- [x] RD-2 evening allowance (app): `evening_drain::catchup_budget_for` / `budget_or` renamed `catchup_allowance_for` / `allowance_or`, returning `CatchupAllowance { budget_secs, frame_cap }` (one read-and-clear under `STATE_IO`); the lane raises the cap before its first catch-up round and logs `frame_cap` on the start and finish lines. Tests: `test_catchup_allowance_for_only_a_fresh_evening_pending_flag`, `a_kill_before_the_pending_flag_is_cleared_ages_out_in_fifteen_minutes`, `allowance_or_applies_to_one_boot_and_fails_closed_on_a_bad_file` (second boot back at 3,000,000).
- [x] RD-3 depth losses deferred, not holding tick segments (storage, app): `wal_deferred_depth::DeferredDepth::defer_lost_range` (every bucket of a lost range, both kinds; `false` on no start, inverted, wider than the table, or a slot collision, which still records an overflow) and `persist_checked` (waits for an in-flight persist, writes, reports the result); `wal_applied_watermark`: the not-landed count split into ticks (`note_unlanded`), depth (`note_unlanded_depth(deferred)`) and deferred depth, `UnlandedSnapshot` / `unlanded_snapshot`, `AckOutcome { Landed, LandedDepthDeferred, TicksLost, TimedOut }` from `wait_for_replay_ack` (losses re-read after the completion is seen; `LandedDepthDeferred` only when every depth loss was deferred and the marks persisted); `unlanded_total` is the tick + depth sum for the pacing wait; `depth_persistence::note_rescue_outcome_depth` marks the range before counting it, in `wal_deferred_depth::lost_depth_marks()` (the process table in production; a separate table under storage's own unit tests so their deliberate rescue failures do not pollute other tests' prunes); `dhan_feed_stack::wal_replay_acked` confirms on `LandedDepthDeferred` with a coded `warn!` `replay_depth_deferred` (depth batches), and logs `replay_rows_lost` (ERROR, pages) apart from `replay_ack_wait_timeout` (the §2.17 source split; `tv_wal_replay_ack_wait_timeouts_total` still counts every refused confirm). Tests: `test_defer_lost_range_marks_every_bucket_both_kinds`, `test_defer_lost_range_refuses_what_it_cannot_hold`, `test_persist_checked_reports_whether_the_marks_reached_the_file`, `test_note_unlanded_depth_and_unlanded_snapshot_count_apart`, the six `test_wait_for_replay_ack_*`, `test_regression_rd3_a_lost_depth_range_keeps_its_archived_segment_and_reaches_the_pass` (bite: drop `defer_lost_range` and the prune deletes the segment), `test_rd3_note_rescue_outcome_depth_does_not_defer_a_sequence_less_loss`, `test_regression_rd3_pass_rewrites_a_lost_depth_range_from_an_archived_segment`, `test_regression_rd3_replay_confirm_maps_every_ack_outcome`, `test_lost_depth_marks_is_the_process_table_in_production`, `test_lost_depth_marks_is_not_the_process_table_under_unit_tests`.
- [x] RD-4 log lines per §2.17 (storage): `wal_replay_frame_cap::cap_is_planned_evening_drain` (a cap above 3,000,000 exists only on the planned evening-drain boot; one process, fails closed) and `refuse_past_frame_cap` logs a coded `warn!` `source = "replay_frame_cap_planned"` on that boot, the coded `error!` `source = "replay_frame_cap"` on every other boot, counter on both. `ws-spill-01` alarm unchanged (its filter matches ERROR only; §2.17 keeps its description unchanged). Runbook rows in `docs/error-runbooks/ws-frame-spill-error-codes.md`. (The `replay_rows_lost` / `replay_ack_wait_timeout` split landed with RD-3.) Tests: `test_regression_rd4_planned_cap_refusal_warns_and_unplanned_errors` (JSON lines as the shipped layer writes them, checked against the deployed filter pattern; bite: always-error fails the planned case, always-warn fails the unplanned case), `test_cap_is_planned_evening_drain_only_above_the_standard_cap`.
- [x] RD-5 docs: dated 2026-11-08 note under Quote 41 in `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md` (`EVENING_DRAIN_REPLAY_FRAME_CAP`); CLAUDE.md new complexity row (`wal_replay_frame_cap.rs` + the depth-deferral arm), a correction of the PR M row's "about 5 to 8 segments", and a dated note on the drain-hold row (`budget_or` → `allowance_or`, `claim_planned_boot`).

### PR RD review — 2026-11-08: four confirmed review findings on PR RD (app, docs)

**Approved by:** the owner's 2026-11-08 decision card "Quiet on planned restarts (Recommended)" ("Any other start that hits the limit still pages you, and a real data problem always pages."), §2.17 of `dhan-rest-only-noise-lock-2026-07-14.md`, which this item now satisfies; no new quote is needed and no page, alarm, filter or EMF name is added.

#### Design
- RDR-1 (after-close pass): an uncertain bucket older than `UNCERTAIN_CARRY_MAX_DAYS` whose readable segments still hold it is written back from them first; only the unreadable neighbour's share is released (same `deferred_depth_uncertain_expired` line and counter, after the write). A bucket with no readable segment is released as before.
- RDR-2 (disk floor): kept by design (item 45a, ticks over depth below the 5% floor); recorded honestly in §2.17's addendum and the runbook row; no behaviour change.
- RDR-3 (planned mark): `evening_drain::claim_planned_boot` claims the mark in `main` before the WAL is locked and replayed and keeps the verdict in memory; `allowance_or` uses it (or claims itself when no early claim ran). A failed clearing write hands out nothing.
- RDR-4 (paging): the `LandedDepthDeferred` arm logs `error!` `source = "replay_rows_lost"` with `confirmed = true` (pages through `ws-spill-01`, as §2.17 requires); the refused arm carries `confirmed = false`.

#### Edge Cases
A planned boot killed before the claim (seconds after start, before any replay): the mark survives up to 15 minutes, as before. A stale flag is cleared, never claimed. An expired uncertain bucket whose readable read fails or whose flush fails is carried, never released. Two `replay_rows_lost` lines on one boot (one per batch) page once per alarm episode.

#### Failure Modes
The early claim cannot write the state file: standard allowance, coded `DRAIN-HOLD-01` error (fail closed). The after-close sink cannot flush an expired bucket: carried to the next pass (keeps the mark). A deferred depth loss now pages: expected only when the depth spill is full, which is itself a real data problem.

#### Test Plan
`test_regression_expired_uncertain_bucket_is_written_back_from_its_readable_segment` (bite: the old first-visit release writes 0 rows and clears the mark); `test_regression_claim_applies_to_the_one_planned_boot_only`, `test_regression_claim_refuses_the_allowance_when_the_clear_fails` (bite: drop the `return None` on a failed clear); `the_lane_and_main_are_wired_to_the_evening_drain` (claim precedes `lock_wal_dir`); `test_regression_rd3_replay_confirm_maps_every_ack_outcome` (deferred arm is `error!` `replay_rows_lost`, no `replay_depth_deferred`).

#### Rollback
Revert the commit: the pass releases expired uncertain buckets unread again, the allowance is decided after the boot replay, and the deferred-depth line is a `warn!` again. No data format changes.

#### Observability
Unchanged counters (`tv_wal_deferred_depth_pass_total{outcome="uncertain_expired"}`, `tv_wal_replay_frame_cap_stops_total`); `replay_rows_lost` now carries `confirmed`. No new page, alarm, metric filter or EMF name.

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] RDR-1 `crates/app/src/dhan_feed_stack/deferred_depth_pass.rs::run_after_close_pass` reads readable segments of an expired uncertain bucket before releasing it.
- [x] RDR-2 §2.17 addendum + runbook row (disk-floor trade-off recorded).
- [x] RDR-3 `crates/app/src/evening_drain.rs::{claim_planned_boot, claim_planned_boot_at, allowance_or}` + the `main.rs` call before `lock_wal_dir`.
- [x] RDR-4 `crates/app/src/dhan_feed_stack.rs::wal_replay_acked` deferred arm pages as `replay_rows_lost` (`confirmed = true`); guard updated.

### PR Q53 — 2026-11-11: fewer alarms, fewer shipped metrics, no CloudWatch dashboard, raw frames to Glacier Instant Retrieval (terraform, aws-lambdas, common, app, storage tests, docs)

**Approved by:** operator Quote 53 (2026-11-11), recorded first in `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md` and its summary stub: the owner ticked "Cut alarms 93 → ~30 (Recommended)", "Trim CloudWatch metrics (Recommended)", "Cheaper S3 class for old raw frames (Recommended)" and "Delete the CloudWatch dashboard", and kept the $75 stop. Noise-lock §2.21 records the removed pages per family before the code.

#### Design
- Q53-1 alarm review: one row per alarm in `docs/audits/2026-11-alarm-review.md` (what it watches, KEEP/REMOVE, what still covers it). REMOVE only alarms outside the KEPT list (data loss, loss groups, lane-down / socket-parked / ticks-dropped, tokens, budget, disk-full / QuestDB-down, cross-verification); also kept: the two EC2 status checks (self-heal actions), every Severity::Critical coded page, the money-path order pages, the page-delivery and gate watchers. Result: 39 removed, 80 kept in prod (the first draft removed 44; its review put back `errcode-stream-silent` with its two schema filters, `live-universe-fallback`, `tick-spill-quarantined`, `wal-suspension-probe-blind` and `deploy-provenance-blind`, each the only page for a KEPT failure). Removed resources are replaced by a dated tombstone comment; references (outputs, the market-hours `ALARM_NAMES` list, the Muhurat skip list) go with them.
- Q53-2 shipped metrics: the EMF selector keeps only names a kept alarm reads (109 → 46); the `class` declaration and the memory/CPU host metrics go; filters whose only reader is removed go. Local `/metrics` unchanged.
- Q53-3 dashboard: `dashboard.tf` and the `dashboard_url` output deleted; dated notes reverse Quote 37's "dashboard KEPT".
- Q53-4 S3: the `raw-frames/` rule moves current and noncurrent versions to `GLACIER_IR` after 30 days (was `DEEP_ARCHIVE`); no expiration; no new rule for `questdb-partitions/`, `deploys/`, `instrument-snapshots/`, `logs/`.
- Q53-5 aws-lambdas (src): `telegram_webhook::ALARM_PHRASES` loses the rows of removed alarms (the phrase guard requires both directions); the two `DEGRADE_NOT_EMERGENCY` entries stay with a dated note.
- Q53-6 date gate: `terraform_data.quote53_not_before_december` (main.tf) with a `plantimestamp()` precondition refuses any plan before 2026-12-01 00:00 IST unless `var.quote53_early_apply_approved` (default false) is set true with a dated owner go-ahead; deleted in the first change after 2026-12-01.

#### Edge Cases
A removed alarm referenced from another file (ALARM_NAMES, outputs, Muhurat skip list): every reference removed, checked by grep. A did-it-run alarm removed while the Muhurat open skipped it: its skip entry goes with it; the kept `boot-heartbeat-gate-not-invoked` stays skipped, and a guard requires every `_not_invoked` alarm in `ALARM_NAMES` to be in the skip list (second review). A shipped metric read by a metric-math alarm: kept (the selector is derived from every `metric_name =` in terraform). Count-gated depth-minter alarms: kept (count 0, no cost). An object already in Deep Archive: stays there (lifecycle cannot move it up).

#### Failure Modes
A removed alarm whose failure is not in fact covered: each REMOVE row names the cover; the owner reviews the table. Found in review and fixed: five removals had no real cover (see the review's "Corrections" section); `quote53_alarm_review_cover_guard.rs` now fails the build if any of the five, or the date gate, goes. While the date gate holds, every terraform apply fails, not only this change (Risk; merge on or after 1 December). `plantimestamp()` in a precondition is Assumed to evaluate at plan time on Terraform 1.9.8 (documented since 1.5; not run here). Terraform not installed here, so no `terraform validate` ran (Risk); a dangling reference would fail the plan at apply. Glacier Instant Retrieval costs more per GB than Deep Archive (Risk, recorded for the owner).

#### Test Plan
`cargo test -p tickvault-aws-lambdas` (lib + all guards), `cargo test -p tickvault-common --tests`, the storage guards that read `deploy/aws` (`--test` each), the app guards that read `deploy/aws` or the phrase table (`--test` each), `tickvault-core --test telegram_lambda_house_style_guard`, `tickvault-trading --test daily_pnl_emit_wiring_guard`, `cargo fmt --all -- --check`. Plus `cargo test -p tickvault-common --test quote53_alarm_review_cover_guard`. Guards changed only as Quote 53 requires (each change dated in the file).

#### Rollback
Revert the PR: terraform recreates the alarms, filters and dashboard, the selector and host metrics come back, and the lifecycle rule returns to Deep Archive. Objects already moved to Glacier Instant Retrieval stay there (readable at once).

#### Observability
No new page, alarm, filter or EMF name. Every removed alarm's coded log lines, counters and local `/metrics` are unchanged. Read after apply: the alarm count (82 in prod), the December bill.

Second review (2026-11-11, same day), added to this item: Q53-7 the metric trim has no terraform in its path (`deploy-aws.yml` and the morning boot copy `cloudwatch-agent.json`), so test 9 of `crates/common/tests/quote53_alarm_review_cover_guard.rs` fails the build while the trimmed config is in the tree before 2026-12-01 00:00 IST (same `quote53_early_apply_approved` switch); the change merges on or after 1 December. Q53-8 `boot-heartbeat-gate-errors` and `-not-invoked` are KEPT (they watch the gate that arms the kept `boot-heartbeat-missing`): back in terraform, the gate `ALARM_NAMES`, the Muhurat `skip_alarms`, `telegram_webhook::ALARM_PHRASES` (82 to 84 rows, aws-lambdas src), and the two Lambda exemptions removed. Q53-9 the `wal-replay-restore-failed` review row names its real cover (`errcode-ws-spill-02`), pinned by test 10. Crates: aws-lambdas (src and tests), common (tests). Rollback, failure modes and observability as above; test plan adds `cargo test -p tickvault-common --test quote53_alarm_review_cover_guard` (test 9 fails until 1 December by design).

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] Q53-0 rule-file-first: Quote 53 (owner) in the daily-universe file; noise-lock §2.21 (full + summary); dated Quote 37 dashboard notes; aws-budget COST NOTE 2026-11-11.
- [x] Q53-1 alarm review doc + terraform removals.
- [x] Q53-2 EMF selector / host metrics / filters trimmed.
- [x] Q53-3 dashboard and its output deleted.
- [x] Q53-4 raw-frames lifecycle to GLACIER_IR.
- [x] Q53-5 `crates/aws-lambdas/src/telegram_webhook.rs` phrase table and its tests.

### WC — 2026-11-11: daily read-only AWS waste check (aws-lambdas, terraform)

**Approved by:** the owner's 2026-11-11 card "Yes, daily waste check (Recommended)", recorded as noise lock §2.18 in `docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md` (rule file first). Crate changed: `tickvault-aws-lambdas` (crates/aws-lambdas) only; plus terraform, the lambda build list, the workspace `Cargo.toml` / `Cargo.lock` (one re-added crate).

#### Design
A Rust Lambda `aws-waste-check` (`crates/aws-lambdas/src/waste_check.rs`, thin bin `src/bin/aws_waste_check.rs`) runs at 18:07 IST every day. It reads volumes (`available`), owned snapshots older than 30 days, Elastic IPs with no association, and the previous state, one S3 object `waste-check/state.json` in the cold bucket (every finding id, no size cap, plus the IST date each unattached volume was first seen; found with a one-key list and read with one get). A volume is reported once seen unattached in runs 2 IST days apart. At most one Cost Explorer query a day (month-to-date by service, top 5, context only), claimed first with a create-only S3 object `waste-check/claims/<date>-cost-query`; the client has SDK retries off (`clients::cost_explorer_from`). Pure functions decide (`decide`): one message through the alerts topic only when the set gains an id; at most one message per IST day. Before sending, the day is claimed (`waste-check/claims/<date>-message`, create-only) and the state is written as "sent today"; if the send fails the previous state is put back. Names are cut to ASCII letters, digits, spaces and a few punctuation marks; the message stays under the relay's 3,500-character cap. Every run writes `s3://tv-<env>-cold/cost-audit/<IST date>.json`. The AWS surface is a trait (`WasteAws`) so the whole run is tested against a fake.

#### Edge Cases
First run and an unreadable state: every current finding is new, once; unattached volumes are first seen that day and reported two IST days later. Same set, a shrinking set, or no findings: nothing sent. Mixed new and resolved: only the new are named. A second run the same IST day: new items wait for tomorrow (not recorded); no second cost query. Two deliveries at once: the second loses the message claim and writes nothing. A snapshot with no timestamp is reported (age unknown). An address with neither allocation id nor IP is skipped; an id that is not short printable ASCII is skipped. Over 15 new items, or items that would pass the character budget: the rest are counted. Cost amounts that are not numbers are skipped and counted in the audit; a next-page token marks the top-5 list partial (no second paid call). A state of any size keeps every id.

#### Failure Modes
An EC2 or state read fails: coded `LAMBDA-AWS-01`, the run errors, nothing is written, the old state stays. The cost claim or query fails: coded `warn!`, the run carries on without the top-5 line. The message claim fails: coded `LAMBDA-AWS-02`, nothing sent or written. The "sent" state write fails: nothing sent, tomorrow retries. The send is refused by SNS (a 4xx, or never sent): the old state is put back (tomorrow retries); if that also fails the message is missed, never repeated, and the ids are logged and in the audit file. The send's outcome is unknown (timeout, network error, 5xx, lost reply; SNS may have delivered it): the "sent" state is kept, so a miss is possible and a repeat is not. The SNS client and the claim client make one attempt each (SDK retries off), so a delivered publish is never re-sent inside a run and a claim is never retried into its own 412. Audit or plain state write fails: both attempted, the run errors. Missing setting: coded `LAMBDA-CONFIG-01`. Retries are 0. The schedule dropped: the did-it-run alarm fires after 2 missed days. A failed run has no alarm of its own (§2.18 allows none): it shows in the log group and as a missing audit file.

#### Test Plan
In `waste_check.rs`: every finding filter and boundary (first-seen days, 30 days, archive tier, association shapes, dedup, unstorable ids), `unattached_since_map`, `top_services`, `cost_period`, `claim_key`, `claim_already_taken`, `CostOutcome`, `parse_state` (every shape), `state_from` (5,000 ids round trip), `decide` for none / first run / new / same / resolved / mixed / corrupt / already sent today / a 200-id set, `clean` (bidi and zero-width characters), message rendering (the 20-item worst case under the cap with its tail kept), audit, settings, SDK mapping, and the full run against a fake (send then silent, first run waits two days, same-day rerun, already sent today, no findings, read errors write nothing, cost failure still reports, one cost query a day, two deliveries send once, state written before the send, state write failure never repeats, send failure puts the state back, send plus put-back failure misses not repeats, message claim failure, audit failure, corrupt state, resolved item). `clients.rs`: `test_cost_explorer_from_makes_exactly_one_attempt`. `crates/aws-lambdas/tests/waste_check_wiring_guard.rs`: per-statement role parse (no mutating, bare `*`, `NotAction`, or star resource beyond the reads), scoped S3 grants, no second policy on the role in any terraform file, a daily cron outside market hours read from the expression, retries 0, exactly the one did-it-run alarm, with fixtures that show each check bites. Existing guards: alarm phrase coverage, lambda zip list lockstep, log export, log group retention.

#### Rollback
Revert the commit: the Lambda, its role, schedule, alarm, log group and invoke config leave terraform on the next apply; the state object, claim objects and audit files stay in the cold bucket (harmless, never read by anything else); `aws-sdk-costexplorer` leaves the lock.

#### Observability
Lambda logs (coded `LAMBDA-AWS-01/02`, `LAMBDA-CONFIG-01`, one `info!` per run with the cost query outcome), the daily audit file in S3 (decision, send error, cost query outcome and error, first-seen dates), and the did-it-run alarm with a plain-English phrase in the Telegram relay. No metric filter, EMF name or per-resource alarm (§2.18).

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] WC-1 `crates/aws-lambdas/src/waste_check.rs` + bin `aws-waste-check` + `clients::cost_explorer_us_east_1`.
- [x] WC-2 `deploy/aws/terraform/waste-check-lambda.tf` (read-only role, schedule, retries 0), log export entry, build list in `terraform-apply.yml`, alarm phrase.
- [x] WC-3 `crates/aws-lambdas/tests/waste_check_wiring_guard.rs`; CLAUDE.md complexity row; §2.18 implementation note.
- [x] WC-4 review fixes (2026-11-11): state in S3 (no digest), first-seen dates for unattached volumes, day claims for the message and the cost query, `clients::cost_explorer_from` (retries off), state before send with put-back, cost failure non-fatal, errors alarm removed, ASCII-only names, message character budget, guard hardening (statements, attachments, cron parse).
- [x] WC-5 second review fixes (2026-11-11, §2.18 notes (9)-(12)), crates changed: `tickvault-aws-lambdas` (src) and `tickvault-common` (tests only). `clients::{sns_single_attempt, s3_single_attempt}` and `SdkWasteAws::new` (one attempt for the publish and the day claims); `PublishError` + `publish_refused` (put the old state back only on a certain refusal; keep the sent state on an unknown outcome); `clean` drops `: / . @ #` (no link in a tag); the role's logs grant is its own log group only (no `CreateLogGroup`), pinned by `waste_check_wiring_guard.rs` (`OWN_LOG_GROUP`, three new biting fixtures); `aws_waste_check` added to `LAMBDA_ERRORS_ALARM_EXEMPT` in `crates/common/tests/cloudwatch_app_alarms_wiring.rs` with the §2.18 reason. Tests: `test_regression_a_lost_publish_reply_keeps_the_sent_state_and_never_repeats`, `test_publish_refused_only_when_certainly_not_delivered`, `test_publish_error_display_names_the_kind`, `test_regression_sdk_waste_aws_new_sends_and_claims_in_one_attempt`, `test_regression_clean_cannot_form_a_link`, `test_sns_single_attempt_makes_exactly_one_attempt`, `test_s3_single_attempt_makes_exactly_one_attempt`.

### PR RS — 2026-11-11: the WAL replay limit and deferral lines page only when the backlog is stuck (storage, app, docs)

**Approved by:** the owner's 2026-11-11 decision card "Page only if stuck (Recommended)", answered after the 8:33 AM IST SOS "safety capture hit a limit", recorded first as §2.20 of `dhan-rest-only-noise-lock-2026-07-14.md` (full file and stub). No new page, alarm, filter or EMF name.

#### Design
- RS-1 (storage, new module `crates/storage/src/wal_backlog_trend.rs`): a one-line record `v1 <IST date> <count|unknown>` (⚠ 2026-11-11 review fix: `v2 <IST date> <count|unknown> <crc32 hex>`, a checksum mismatch reads unreadable) in `data/state/wal-backlog-trend.state` (temp file, `sync_all`, rename, folder sync); a strict parser (`Absent`, `Unreadable`, `Valid`); a pure verdict `backlog_trend_verdict(previous, today, current)` that is `Shrinking` only when both counts are known, the record is not dated after today and the current count is strictly lower; every other case names its reason. `judge_at_boot(wal_dir)` runs once per process (a `OnceLock`), counts, decides, overwrites the record (removes it when the write fails) and sets one process `AtomicBool`; `judge_at_boot_in` takes the paths and the slot for tests. ⚠ CORRECTED 2026-11-11 (review): the verdict is kept in a `OnceLock<BootJudgement>`; the only `AtomicBool` is the log-once latch; there is no injectable slot (`boot_backlog_trend()` is process-global, and the storage lib tests rely on none of them calling `judge_at_boot`). `judge_at_boot_in(wal_dir, state_path, today_ist_day, planned_evening_restart)` takes the paths, the day and the planned flag. Review fixes: the planned evening-drain restart saves no count (`judge_at_boot` reads `wal_replay_frame_cap::planned_evening_drain_boot()`, whose claim `main` records before this call), so other starts compare with the previous non-planned start; a failed save makes the verdict `BacklogTrend::StateNotSaved` (pages) via the pure `trend_after_save(compared, saved)`.
- RS-2 (storage, `wal_unapplied_ranges`): `waiting_unreplayed_segments(wal_dir)` counts the waiting segments (the same span scan as `backlog_segments`) the watermark saved on disk does not cover (`AppliedSnapshot::segment_is_applied`); with no loadable watermark every waiting segment counts; `None` when the waiting set cannot be read. `AppliedSnapshot::load_quiet` becomes `pub(crate)` so the boot count does not report a rejected watermark twice. ⚠ 2026-11-11 second review fix: while segments wait, a watermark that is absent, rejected, or overflowed with no known height gives `None` (saved as `unknown`), never an inflated count; `wal_backlog_trend` logs which record a failed save removed (`removed_record`, `removal_synced`) and runs `judge_at_boot` through `off_worker`; the stated cost now includes the span scan's gap tail reads (64 KiB, at most 4 MiB per gap segment). Tests: `test_regression_untrusted_watermark_gives_an_unknown_backlog_count`, `test_watermark_bounds_the_count_only_for_a_known_height`, `test_regression_an_untrusted_watermark_saves_unknown_not_an_inflated_count`, `test_regression_a_failed_folder_sync_reports_this_starts_record_removed`, `test_removed_record_label_names_the_record`.
- RS-3 (storage, `wal_replay_frame_cap`): a `ReplayLimitVerdict { Page, PlannedRestart, BacklogShrinking }` from `replay_limit_verdict(planned, shrinking)` (⚠ CORRECTED 2026-11-11: the signature is `replay_limit_verdict(planned: bool, trend: Option<BacklogTrend>)`) (planned wins); the three reporters take it; `_shrinking` sources; the memory-guard stop keeps the ERROR on every start; `replay_limit_verdict_this_boot()` reads both process flags.
- RS-4 (app): `main.rs` calls `judge_at_boot` after `lock_wal_dir` and before `replay_all_fenced` (one line, the K4 comment shortened so `main.rs` stays at its ceiling); the catch-up drain passes `replay_limit_verdict_this_boot()`.
- RS-5 docs: runbook rows in `docs/error-runbooks/ws-frame-spill-error-codes.md`; dated note on CLAUDE.md's `wal_replay_frame_cap` row.

#### Edge Cases
First start ever (no record: pages); record unreadable, wrong version, garbage, negative or too-long count, trailing junk (pages); record dated after today, e.g. a clock step back (pages); current count unknown (unlistable folder: pages and records `unknown`); previous `unknown` (pages); equal count (pages); higher count (pages); count 0 now and 1 before (quiet; nothing left is fine); planned evening start (planned wins, `*_planned`); memory-guard stop (always ERROR); two starts the same day (compared normally); the state write fails (record removed so the next start pages); `data/state` missing (created).

#### Failure Modes
A wrong "shrinking" verdict quiets one start's lines only (counters and `warn!` remain; the next start compares again). A failed write that also cannot remove the old record leaves the older count, which can read as shrinking on the next start: Risk, logged once as a coded `warn!` (`source = "wal_backlog_trend_state"`). ⚠ CORRECTED 2026-11-11 (review): that was not "the next start" only; with the folder unwritable EVERY later start compared against the frozen count. Fixed: a start whose save fails pages (`state_not_saved`), so an unwritable folder pages on every start; after it recovers, at most one start can compare against a left-behind record. Also fixed: the planned evening restart's count hid day-over-day growth (it no longer saves), and a corrupted digit could read as a valid higher count (CRC-32 added). The count reads segment headers (O(waiting segments), cold, once per start) before the boot replay.

#### Test Plan
Pure verdict permutations (first start, unreadable, future date, unknown now, unknown before, shrink, equal, grow, zero); record round trip and every corruption form; `judge_at_boot_in` end to end in a temp folder (first start pages and records; second start lower is quiet; equal pages; unlistable folder records `unknown`; unwritable record path removes the old one); review regressions `test_regression_planned_evening_restart_does_not_hide_day_over_day_growth`, `test_regression_an_unwritable_state_folder_pages_on_every_start`, `test_regression_a_flipped_digit_in_the_record_reads_unreadable_not_higher`, `test_trend_after_save_pages_on_a_failed_save`; the 8:33 AM regression (morning start lower than the last evening start: quiet `_shrinking`, not matched by the `ws-spill-01` filter read from Terraform); reporters under the JSON subscriber for every verdict, memory guard included; `waiting_unreplayed_segments` with and without a watermark and on an unlistable folder; wiring assertions in `the_lane_and_main_are_wired_to_the_evening_drain`. Scoped `cargo test -p tickvault-storage --lib`, `cargo test -p tickvault-app --lib`, clippy, fmt, `source_scan_budget_guard`, `alarmed_counters_are_seeded_guard`, `source_size_ratchet_guard`.

#### Rollback
Revert the commit: the lines page on every unplanned start again (§2.17). The state file is ignored by the old code and harmless.

#### Observability
Every counter unchanged on both paths; the boot logs one `info!` line with the previous and current counts and the verdict reason; the quiet lines carry `previous_backlog_segments` and `backlog_segments`. Local logs and `/metrics` only.

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] RS-0 rule-file-first: §2.20 in the noise lock (full file and stub bullet); this plan item.
- [x] RS-1 `crates/storage/src/wal_backlog_trend.rs`.
- [x] RS-2 `crates/storage/src/wal_unapplied_ranges.rs::waiting_unreplayed_segments`.
- [x] RS-3 `crates/storage/src/wal_replay_frame_cap.rs` verdict and reporters; `crates/storage/src/ws_frame_spill.rs` call site.
- [x] RS-4 `crates/app/src/main.rs` boot call; `crates/app/src/dhan_feed_stack.rs` catch-up call site; `crates/app/src/evening_drain.rs` wiring test.
- [x] RS-5 runbook and CLAUDE.md notes.

### DD — 2026-11-11: one-off delete of the four Quote 52 leftovers (app, terraform, one workflow)

**Approved by:** the owner's 2026-11-11 card "One-off, these 4 only (Recommended)", recorded as Quote 56 in `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md` and §0.19 in `docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md` (rule files first). Crate changed: `tickvault-app` (crates/app) src; `tickvault-common` tests only (the workflow budget row).

#### Design
`crates/app/src/disk_delete_2026_11_11.rs` holds the four ids as constants (`VOLUME_IDS`, `SNAPSHOT_IDS`) and `run(aws, mode, now)`: first reads the live root `vol-07a6586a56466ea83` and refuses unless it is visible and attached (a wrong account or region would read every leftover as gone); per volume, read it; delete only when `available` with no attachment; per snapshot, delete when this account still owns it; `dry` sends `DryRun` (proves the permission, deletes nothing); `apply` deletes then reads every deleted id again up to 10 times, 3 s apart, and fails unless each is gone (`deleting` counts as gone). Refused at and after 2026-11-14 00:00 UTC. The AWS surface is a trait (`DiskAws`) with the EC2 client behind `SdkDiskAws`. Thin bin `tv-disk-delete-2026-11-11`. `deploy/aws/terraform/disk-delete-2026-11-11.tf`: role `tv-prod-disk-delete-2026-11-11`, trusted only by the GitHub environment `disk-delete-2026-11-11`, delete actions on the four exact ARNs, describe on `*`, every statement and the trust expire at 2026-11-14 00:00 UTC. `.github/workflows/disk-delete-2026-11-11.yml`: dispatch-only, `dry` or `apply`, two `run:` lines (build before credentials, then run).

#### Edge Cases
Already deleted (the owner may still delete in the console): reported "already gone", success. A volume found attached or `in-use`: not deleted, run fails. Past the deadline: refused before any AWS call. A second `apply`: every id already gone, success. AWS returns `deleting` after the delete: counts as gone.

#### Failure Modes
Role not applied yet: the credentials step fails, nothing runs. Permission refused: the run fails naming AWS's code. A read fails: that id fails, nothing deleted for it. A snapshot that still exists after 10 reads: verify fails. The live root is never in the lists (test). The role cannot delete anything else (exact ARNs).

#### Test Plan
In the module: ids exactly the four and never the live root; deadline instant; mode parsing; deletable and gone predicates; full runs against a fake: wrong account or region refused (live root not visible), live root not attached refused, a re-run finding `deleting`/`deleted` succeeds with no call, a volume that never disappears fails verify, dry deletes nothing, apply deletes all four and never touches the live root, second apply, attached volume refused in three shapes, past-deadline refusal with no call, permission refusal in both modes, unreadable volume, `deleting` read, a snapshot that never disappears, mode mismatch, `SdkDiskAws::new`. Guards: `shell_budget_guard` (the two-line row), workflow guards (pinned actions, no dispatch input in `run:`).

#### Rollback
Revert the commit before the run: the role leaves terraform on the next apply. After the run the deletes cannot be undone (owner-approved, Quote 56).

#### Observability
The workflow log prints one line per id and `result: OK` or `FAILED`; a read-only describe afterwards confirms; the daily waste check reports any leftover at 6:07 PM.

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] DD-0 rule-file-first: Quote 56 (full + summary), rust-only §0.19 (full + summary).
- [x] DD-1 `crates/app/src/disk_delete_2026_11_11.rs` + bin `tv-disk-delete-2026-11-11`.
- [x] DD-2 `deploy/aws/terraform/disk-delete-2026-11-11.tf` + `.github/workflows/disk-delete-2026-11-11.yml` + budget row.
- [x] DD-2b fix (2026-11-11): the role tag `Purpose` held brackets, which IAM refuses at apply (run 43046930995, nothing else applied); now `... leftovers - Quote 56`. New guard `crates/common/tests/terraform_tag_value_guard.rs` checks every literal tag value in `deploy/aws/terraform/*.tf` against the AWS tag character set, with a fixture that bites on the old value.
- [x] DD-3 after merge: terraform apply (run 43062788421), `dry` (43068890684) then `apply` (43069827741), all four deleted and confirmed by a read-only describe; this PR deletes the role, workflow, tool and row and records Quote 56 ENDED.

### LF — 2026-11-12: live signal timing, paper order path, per-order event log and daily calibration report (trading, storage, app, deploy)

**Approved by:** the owner on 2026-11-12, "go ahead build everything dude", after the four decision cards recorded as Quote 57 in `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md` and §2.22 of the noise lock (rule files first). PAPER ONLY: no real order, probe order, modify or cancel; no `dry_run` / `[exit_orders]` / live switch flipped; no strategy logic of tickvault's own; strategy logic stays brutex's pinned crate behind `brutex_host::engine::StrategyEngine`. Crates changed: `tickvault-trading` (new modules under `crates/trading/src/brutex_host/`), `tickvault-storage` (one new table writer), `tickvault-app` (paper wiring, clock and handshake samplers, daily report), `tickvault-common` (tests only), `deploy/aws/terraform` (one PutObject grant on `live-fill-logs/`).

**Measured basis (2026-11-12, recorded in the session):** F&O cash stocks, every November trading day: a new price reaches the server p50 0.75 s, p90 1.27 s, p99 1.70 s after the start of its exchange second; 22–26% after the second ends; the fastest stock per second p50 0.04 s. Network to Dhan 3.5 ms TCP round trip, TCP+TLS 13–22 ms. Clock 0.6 µs off Amazon time (±81 µs), no `/dev/ptp` on r8g.large. Exchange trade time is whole seconds.

#### Design
- **LF-1 `brutex_host::live_candles`** (pure): per `(InstrumentKey, frame)` one open bar in a pre-sized dense table (index map built at configure). Frame boundaries aligned to IST midnight (5 s: `:00.000–:04.999`). A tick belongs to bucket `floor(exchange_secs / size)`. Close triggers: (a) the clock: `on_clock(now_ns)` closes every open bar of a frame whose boundary has passed (all instruments share the boundary, so this is O(open bars of that frame) once per boundary, flagged, not O(1)); (b) a tick stamped at or after the boundary closes that instrument's bar first (O(1)). Each closed bar carries `close_by` (Clock | Tick), `clock_close_ns`, `first_next_tick_receipt_ns`, `last_tick_exchange_secs`, `last_tick_receipt_ns`. A tick stamped before the current open bucket (late) never enters a closed bar; counted. Zero allocation after configure; O(1) per tick.
- **LF-2 `brutex_host::worst_fill`** (pure): the Brutex worst-case fill model. Inputs: decision second, side, quantity, per-second `{high, low, volume}` for the decision second and later seconds, best bid/ask with quantities at the decision, tick size, the 25% participation cap. Per fill second price = worst of (that second's high for buy / low for sell, the next second's, the decision second's, the best quote), shares beyond the best-quote quantity priced at the next worse of those, then one tick against; each second fills at most 25% of its volume; the remainder rolls on. Output: the fill schedule (second offset, qty, price), the average, the unfilled remainder. O(seconds used) per order, no allocation into a caller-owned buffer.
- **LF-3 `brutex_host::order_price`** (pure): the marketable LIMIT price: buy `min(ltp × (1 + band), slot × 5 / qty)` rounded DOWN to the tick, sell `ltp × (1 − band)` rounded UP to the tick, `band` = 0.10 from config, clipped to the day's price band when known; refuses NaN/zero/negative inputs. O(1).
- **LF-4 `brutex_host::send_queue`** (pure): a fixed-capacity priority queue for intents waiting on the order rate limits, ordered by the strategy's rank then arrival; records the wait. O(log n), n ≤ capacity (64), flagged.
- **LF-5 storage `live_order_events`**: one row per event (decision, built, sent, ack, exchange order time, fill, reject, modify, cancel, exit, stop, square-off) with millisecond IST and exchange times, recipe, version, signal id, instrument, side, qty, slot, candle start/end, last tick in candle, close trigger, book at decision, feed lag, API round trip, limit price, queue wait, fill qty/price, remainder, reason, charges; DEDUP `(ts, signal_id, event, seq, feed)`. Off the hot path through a bounded channel; ILP like `order_audit`.
- **LF-6 app paper wiring**: an O(1) `try_send` tap beside `consume_tick` in `LiveIngest` into a bounded pre-allocated channel; a strategy task owns the configured `StrategyEngine` (none configured today: the task idles and says so), the strategy candles, the per-second state and the send queue; intents become paper orders on the existing `order_runtime` OMS (no second `spawn_order_runtime`); the would-be fill comes from LF-2 on live per-second data; every step writes LF-5 rows. Clock offset (chronyc) every minute into `host_capacity_minute` (new column) and the page in §2.22; a TCP+TLS handshake to the feed, order and order-update hosts 5 times per session into a new `network_rtt_session` table.
  - **Recorded 2026-11-12 (LF-5 review):** nothing starts the `live_order_events` boot ensure or its disk-tier drain yet. Where the paper chain builds `LiveOrderEventsWriter::new`, first await `ensure_live_order_events_table` and call `spawn_audit_spill_drain_once(AuditSpillTable::LiveOrderEvents, ..)`, as `crates/app/src/order_observability.rs` does for `order_audit`, with a source-scan ratchet that fails the build if either call is missing beside the writer's construction.
- **LF-7 daily calibration report**: after 15:40 IST, from LF-5 rows plus `ticks`/`tick_quotes`/`market_depth_book`: fill-second distribution from the decision, live or paper price vs the LF-2 worst case per order, every worse-than-worst fill flagged loudly (coded ERROR, full row), step latency p50/p95/p99/max, feed lag, rejects, partials. JSON + CSV to `s3://tv-prod-cold/live-fill-logs/YYYY-MM-DD/` (PutObject on that prefix only).
- **LF-8 replay**: a CLI replays one recorded day from the archived `ticks` CSV through LF-1/LF-2 with a TEST FIXTURE intent source (never a strategy, never on the live path) and prints every decision time against its candle boundary, plus a sample log and report.

#### Edge Cases
Feed gaps; duplicate, out-of-order and late ticks (late = stamped before the open bucket); clock skew and steps (both directions); a reconnect mid-candle; empty seconds and empty candles (no bar emitted, counted); crossed, zero or one-sided quotes (worst fill uses the trade side only, flagged); trading halts and circuit limits (band clip; an order the band refuses is a would-be reject); Dhan protection rejects; rate-limit errors (queue); API timeouts and outages; token expiry; partial fills across many seconds and an exit that never fills (remainder carried to 15:10 square-off, then reported unfilled); freeze quantity (split, each leg logged); F&O ban days; expiry days; holidays and Muhurat (calendar-driven boundaries); the 09:15:00.000 and 15:29:59 edges; Dhan's own intraday square-off; tick-size or lot-size changes between days (read at configure); a 1 s frame where the trade second and the boundary coincide; a tick with exchange time ahead of receipt; volume 0 seconds (25% of 0 = no fill that second); u32 cumulative-volume wrap; NaN/inf prices; an instrument with no ticks all day.

#### Failure Modes
Strategy channel full: the tick is dropped from the STRATEGY path only (storage path unaffected), counted, coded warn once per episode. Event writer down: rows retained in a bounded buffer then spilled like `order_audit`; counted. Clock sampler fails: gauge absent, coded warn, no page (the page needs readings). Report upload fails: retried 3 times, file kept on the box, coded ERROR. A worse-than-worst fill: coded ERROR with the full row (never silent). No strategy configured: the chain idles; the report says "no orders today".

#### Test Plan
Every LF module ships tests written to fail first: unit tests per edge case above; property tests (random tick streams vs a brute-force reference for LF-1 close timing and bucket membership; LF-2 against an exhaustive reference on small inputs; LF-3 rounding never crosses the band or the cap); DHAT zero-allocation for LF-1 per tick and LF-2 per order; Criterion benches printing p99 for LF-1 per tick, LF-2 per order, LF-3, LF-4; integration tests for LF-5 DDL and DEDUP, the LF-6 tap (no allocation, no blocking), LF-7 report from fixture rows; source-scan guards that no LF path places a real order or flips a live switch.

#### Rollback
Each LF PR reverts on its own; the tap is behind `[live_fill] enabled` (default false until the paper chain is reviewed live), the report behind the same switch; the table and the S3 prefix stay (harmless).

#### Observability
`live_order_events`, `network_rtt_session` and the new `host_capacity_minute.clock_offset_ns` column in QuestDB; the daily report in S3; counters for strategy-path drops, late ticks, close-by-clock vs close-by-tick, queue waits; the §2.22 page; the status page.

This item carries the 15-row + 7-row guarantee matrix by reference to `.claude/rules/project/per-wave-guarantee-matrix.md`.

- [x] LF-0 rule-file-first: Quote 57 (full + summary), noise lock §2.22 (full + summary), this item.
- [x] LF-1 `brutex_host::live_candles` + tests + DHAT + bench.
- [x] LF-2 `brutex_host::worst_fill` + tests + DHAT + bench.
- [x] LF-3 `brutex_host::order_price` + tests + bench.
- [x] LF-4 `brutex_host::send_queue` + tests + bench.
- [x] LF-5 storage `live_order_events` table and writer.
- Owner decisions for LF-6b (2026-11-12, recorded in the Quote 57 addendum): skip and log an order from a candle that took a late price; cancel a partial remainder; 60 s fill limit; forward 1 s closes for instruments with an open paper position to the paper risk engine. Synthesized design: scratchpad lf6b-plan.md (judge of three designs).
- [ ] LF-6 app paper wiring, clock offset column and page, handshake RTT table.
  - 2026-11-12: LF-6a merged (#2238; clock offset column, §2.22 page, handshake table; the paper wiring is LF-6b): `host_capacity_minute.clock_offset_ns` (`crates/storage/src/host_capacity_minute.rs`, fed by `crates/app/src/infra.rs`), `ClockOffsetEpisode` + `CLOCK-SKEW-01` (`crates/app/src/clock_offset_page.rs`, `crates/common/src/error_code.rs`, `deploy/aws/terraform/error-code-alarms.tf` `clock-skew-01`, runbook `docs/error-runbooks/clock-offset-and-network-rtt-error-codes.md`), `network_rtt_session` (`crates/storage/src/network_rtt_session_persistence.rs`, `crates/app/src/network_rtt.rs`, `NET-RTT-01` log-only, `tls::shared_http11_client_config` in `crates/core/src/websocket/tls.rs`).
  - 2026-11-12: LF-6b built, not merged (the live paper fill chain; LF-6 stays unticked until it merges): tap `crates/app/src/dhan_feed_stack/live_fill_tap.rs` (Folded arm, live socket and not `replaying_wal`, busy-polled `frame_ring`, `drop_seq`), chain `crates/app/src/live_fill/{mod,chain,timing,taint,fill_window}.rs` (`tv-live-fill` thread, generic over `StrategyEngine`; per-frame 09:15:00 clamp of the book clock to `end − 1 ns` until `end + open_wait`; release at `end + W(bar)`; drop / feed-gap / clock-step taint; held order with the worst fill worked out once, 60 s window; `skip:late_price`; remainder cancelled; 1 s marks for paper positions), runtime arm `crates/app/src/order_runtime_live_fill.rs` (dry-run only, `PAPER-` ids, `PART_TRADED` + cancel), event writer `crates/app/src/live_order_events_boot.rs` (ensure, then disk-tier drain, then writer; source-scan ratchet), `[live_fill] enabled = false` (`LiveFillConfig`), `LiveCandleBook::last_tick_late`, DHAT `dhat_live_fill`, bench `live_fill` (keys `live_fill_tap`, `live_fill_chain`). `select_engine()` returns `None` (no approved brutex crate), so production idles. Open: the per-contract tick/lot/label map from the day's artifact is not built (no engine to use it); no partial fill survives a restart (held orders are RAM only).
  - 2026-11-12: LF-6b review round 2 (3 passes, 5 lenses, 2 refuters per finding): 22 more confirmed findings fixed (socket-loss taint, fresh first trade, older real trades, exits capped to the held position, partial first bars, taints over fill seconds, 15:40 gap watch, exit retries and the rate-limit page once per episode, outbox room, live_shed.rs split for the size pin). Bench (x86 dev container): tap 25.5 ns median, chain 101 ns per tick median, 150 ns per tick p99 of sample means. Owner set the opening wait to 1 s (Quote 57 addendum, card "1 s, same as all (Recommended)").
- [ ] LF-7 daily calibration report to S3 + IAM grant.
  - 2026-11-12: LF-7 built, not merged (stays unticked until it merges; built on the LF-6b head): `crates/app/src/live_fill_report/{mod,rows,recheck,build,render,upload}.rs` (attempts 15:47, 15:54, 16:01 IST on trading days, 300 s each; readiness = seal barriers + `wal_tables()` applied for `live_order_events` and `candles_1s` + empty event spill; re-prices each order from stored `candles_1s` through the moved `live_fill::fill_window::price_held_window`; five files in `data/live-fill-logs/<D>/` copied with `raw_frame_upload::upload_file` to `live-fill-logs/<D>/`; marker `live_fill_report` only when all five verify; leftover days up to 3 trading days back), four Decision-row columns on `live_order_events` (`tick_paise`, `lot_size`, `participation_bps`, `max_fill_seconds`), table `live_fill_report_daily` (`crates/storage/src/live_fill_report_persistence.rs`, DAY partition, DAY retention list), `live_fill::last_boot_outcome`, LIVE-FILL-01/02 log-only (runbook `docs/error-runbooks/live-fill-report-error-codes.md`), IAM: an explicit Deny of DeleteObject/DeleteObjectVersion on the prefix (no new Allow: the whole-bucket grant already covers PutObject). **Deviation from the line above:** the report reads `live_order_events` plus stored `candles_1s`, not `ticks`/`tick_quotes`/`market_depth_book`, because the chain prices on 1 s bars and the best quote only; and the paper rows' own `worse_than_model` is always `not_worse`, so the check is a re-pricing, not that column. Wiring: `live_fill_report::start` from `main` (spawns only with `[live_fill] enabled`). Limits: paper only (chain vs stored data, not a real fill); a catch-up day after `candles_1s` is archived reads Amber (`recheck_unavailable`); the re-pricing cuts at the trigger second's successor as the chain usually does.
  - 2026-11-12 (LF-7 review, 14 findings, still unticked): an exit cut to the held position is judged on the Sent quantity; `process_started_after_0900` only for a start inside the day's session; today's attempts first, leftovers after and only while a whole run stays outside 09:00–15:40 and the stop window, one late final attempt after 16:01; every step budgeted inside a 420 s attempt; the summary row written before the copy and upserted after it (designated `ts` = the day, `reported_at` column); no row unless the table is ensured; the day read cap 50,000 rows with the body freed early and the parse off the worker; a stored `max_fill_seconds` bounded by the config ceiling (600); tests renamed to name every new pub fn. Runbook stages added: `db_row_not_ensured`, `db_row_from_files`, `copy_timeout`.
  - 2026-11-12 (LF-7 review round 2, 8 findings, still unticked): runtime refusals (`risk:`, `place_refused:` incl. given-up exit retries, `refused:`, `cancel_refused:`, `fill_not_applied`, runtime `exit_without_position` after a Built row) read `rejected` with no Sent row, the fixture now writes `refused:non_paper_id` without one; a `fill_model_refused` reject is priced as filling nothing (stored data that fills it reads `qty_differs`, Red); the 500-order cap and the wall budget count only the orders the report compares (`build::priced_signal_ids`); the copy uses the new marker-free `raw_frame_upload::copy_file_verified` and writes its own per-file record, a failed record reported only as LIVE-FILL-01 `stage="marker"`, never on the paged `cold-backup-failing` counter; a `copy-started` record stops every later rebuild of a day once its copy began (`copy_started`, `rebuild_wanted`; stage `rebuild_after_partial_copy`); `live_fill_report_daily` added to both console SEBI export lists, offset by moving the shell comment block into Rust comments (console shell still under its ceiling, no ceiling raised); the cost wording corrected: each re-pricing's reply is at most `max_fill_seconds + 3` rows but the server filters every instrument's `candles_1s` rows in the window (`security_id` unindexed), and the day read's LIMIT does not bound the server sort.
  - 2026-11-12 (LF-7 review round 3, 3 findings, still unticked): (1) a no-orders day reads Amber (`partial`, reason `live_fill_not_running`) unless live fill ran in this process (`build::live_fill_ran`: `Running` or `IdleNoEngine`); (2) a day folder older than the three-trading-day lookback whose five files have no copy record (`upload::folder_copied`, never swept, unlike the 7-day daily marker) is found each pass by `upload::unreported_day_folders` (O(folders), once a pass, `off_worker`) and COPIED as is, never rebuilt, at most `OLD_DAY_COPIES_PER_PASS` (3) a pass; an incomplete one logs LIVE-FILL-01 `stage="outside_lookback_incomplete"` once per process; runbook sentence corrected (it promised a copy the old code never made) with the manual remedy; (3) a run still owed (today unmarked, or a leftover or old day deferred) when the loop ends inside 15:40–16:45 wakes at the stop window's end the same day (`wake_after_stop_window`, O(1)) instead of the next day. Also fixed: the percentile proptest's f64 reference (0.68 × 150 read as 102.00000000000001, ceiled to 103) is exact integer arithmetic; the saved regression case is kept.
- [ ] LF-8 replay CLI, sample log and report.
  - 2026-11-12: LF-8 built, not merged (stays unticked until it merges; built on the LF-7 head). **Input source changed (dated amendment to the LF-8 line above):** the replay reads the day's raw WAL segments (a local folder, or `s3://tv-prod-cold/raw-frames/<D>/` with List and Get only), not the archived `ticks` CSV, because the CSV has no best bid/ask (the LF-2 worst-case model prices from the best quote), no new-trade flag, receipts only to the microsecond, and is archived days later; the raw segment is the frame as it came off the socket, so the replay walks it with the live drain's own code. The owner approved the CSV wording; this change is stated in the PR body. Files: `crates/app/src/live_fill/replay/{mod,source,drive,scripted,out,replay_tests}.rs`, binary `tv-live-fill-replay` (`crates/app/src/bin/tv_live_fill_replay.rs`), `crates/app/src/live_fill/paper_rows.rs` (the runtime's paper order rows, lifted out of `order_runtime_live_fill.rs` unchanged and shared), `crates/app/src/dhan_feed_stack/fold_verdict.rs` (the drain's per-tick verdict, moved unchanged out of `ingest_tick_at`, pinned by a proptest against the old expressions; `tick_row_retention_guard.rs` now reads the moved binding), `crates/storage/src/raw_frame_fetch.rs`, `live_order_events_persistence::{write_csv_header, write_csv_record}`, golden day `crates/app/tests/live_fill_replay_golden.rs` (WAL written at test time, fixtures in `tests/fixtures/lf8_golden/`), isolation guard `crates/app/tests/live_fill_replay_isolation_guard.rs` (4 rules, a bite test each). Outputs: `decisions.csv`, `events.csv`, the five LF-7 report files from LF-7's own builder (re-priced from the replay fold's 1 s seals), `summary.txt` (with the honest limits and file hashes), and `diff.csv` with `--compare`. Paper only: no order, no broker, no database row, no bucket write, no new error code, page, alarm, IAM, Terraform or config change, no new dependency. Deviations from the judged plan: passes only on the grid (no extra pass at each receipt); the grid also ends at the last frame plus the longest frame, the wait and the fill window (a partial day is not driven to 15:30); the elapsed time is printed, not written into `summary.txt` (it would break byte-identical output); the chain starts at the day's first frame (as the live chain starts at boot), so a WAL that starts after 09:15 marks the first candles `chain_start`.
  - 2026-11-12 (LF-8 review, 16 findings, still unticked): (1) a loss is marked where it happened: the first frame of a segment that was resynced at its head now moves the loss count (no applied watermark, so `after_gap` there can only be damage), a segment whose damage was all resynced past is counted but never bumped a second time after it, and only a walk that STOPPED early (`SegmentRead::stopped_early`, new storage field) or a trailing skip bumps after the segment; (2) every loss the replay sees reaches the chain at once through `LiveFillChain::note_tap_loss`, as the live thread reads the tap total before every pass; (3) exit 3 reads the LF-7 re-pricing (`worse_than_worst` + `stored_worse`, `self_check_failures`), the old row check could never fail; (4, 5) the isolation guard bans the bucket's delete, copy and upload calls, the paging types and `Level::ERROR`, allowlists the storage and report modules the replay may name, and catches grouped imports, `super::replay` inside `live_fill/`, renames and globs, each with a bite test; (6, 11) a local folder holding a segment twice is refused (exit 2) and one with a `.sha-` sidecar copy too (exit 3); (7) `--staging` must be new or empty and the bucket fetch creates its `.gz` with `create_new` (`raw_frame_fetch::reserve_new_file`), never overwriting or removing a file it did not make; (8) `tick_row_retention_guard` also pins that `ingest_tick_at` calls `fold_verdict::fold_verdict` and binds none of the three verdicts itself; (9) a day whose first frame came after 09:15 is incomplete (exit 2); (12) `--wal-dir` reads only the segments named on the day plus the newest before it; (13) a `bar_open_ist` off the frame's grid or outside the session is refused; (14) `--compare` is read before any segment; (15) the segment-split proptest now writes real segments with the live WAL writer and reads them back through `feed_segment`; (16) the stated memory now includes the fold and the never-pruned 1 s bars, and "largest segment read" is the unzipped size. The golden's only change: loss count 2 to 1 (the double bump), and the honest-limits wording; its segments are now named on the replayed day.
  - 2026-11-12 (LF-8 review round 2, 10 findings, all judged real, still unticked): (1) a loss found before the day's chain exists (the previous date's damaged segment, damage at the head of the day's first segment) no longer moves the loss count; it is counted apart (`losses_before_chain`, printed), as the live tap takes its baseline at creation, so the 09:15 bars are no longer tainted `tick_dropped` for a loss live never saw; (2) honest limits: a main-feed socket outage on the recorded day is invisible to the replay (its bars are built short and trusted, where live refuses them), and the first limit no longer calls the output "the no-loss view"; reading the socket audit to reproduce outages is NOT built; (3, 4) the isolation guard is now an allowlist of every path the replay names under `crate::`, `super::`, `tickvault_*` and `live_fill_report::` (groups expanded, renamed roots and items refused), reads paths with whitespace squeezed, flags `live_fill::{self as x}`, and strips comments with a lexer that keeps a `//` inside a string; every bypass in the finding was run against the old guard (zero violations each) and is now a bite case; (5) the Z11a reader tests moved to `ws_frame_spill/tests/z11a_tests.rs`, so `ws_frame_spill.rs` is 13,486 lines, under its 13,667 ceiling (pin not raised; it cannot be lowered below the current count); (6) the in-session silence is clamped at 09:15 and 15:30, so a hole across the open or the close marks the day incomplete; (7) an odd `--pass-us` gets one pass at exactly the grid's end, before the frame that reached it is tapped (a pass in `finish` alone ran after later frames and tainted the order), and at an early-stopped day's tail end; (8) `--compare` matches on candle start and end and keeps one row per decision (the export query selects `candle_start`); (9) the bucket listing refuses a `.sha-` copy of the previous date's segment it reads; (10) an `every_bar` frame with no bar inside the session is refused (`check_every_bar`). Each fix has a test that failed with the fix reverted. The golden's only change: the summary's new loss line, the two honest-limit lines and the compare query.
  - 2026-11-12 (LF-8 review round 3, 8 findings, all judged real, still unticked): (1) a process restart on the recorded day is not reproduced (one chain, fold and paper position for the whole day, where live starts fresh and flat at each boot): a new honest-limit line says so, and an in-session silence over 30 s (`RESTART_HINT_GAP_NS`) is named as a possible restart in the incomplete reason; rebuilding the chain on a silence is NOT built, because a feed outage without a restart would then reset a real paper position, and no recorded frame says which it was; (2) a local folder now picks the previous segment by the bucket's rule (the newest named on D-1 only, so none after a weekend), pinned by a parity test against `plan_day_listing`; (3) the guard reads a leading `::` (and `use ::x`, squeezed by `normalize`) as a path root; (4) the AWS SDK named directly (`aws_sdk_`, `aws_config`, multipart and bucket calls) is refused in the replay and in the bucket reader outside its tests; (5) `tickvault_storage` and `tickvault_common::config` allow exact items only (never a whole module), `questdb` is refused in any case, and only the replay's tests may name `WsFrameSpill`; (6) a day with no segment named on it is refused in both sources (the bucket's before any download), no frame of the day exits 2 even with `--allow-partial`, and a run with no decision is incomplete and its report Amber (`settled` false); (7) a duplicate or `.sha-` copy of a segment the day does not read no longer blocks the folder (`select_day_inputs` refuses only what it reads, as the bucket does); (8) the feed-gap pin now streams a busy contract OUTSIDE the universe. Each fix has a test that failed with the fix reverted (the guard's three by reverting each in turn). The golden's only change: the summary's new restart limit line. The bucket-side `day_segments` refusal is covered by the storage listing test only (no bucket in tests).
  - 2026-11-13 (LF-8 review round 4, 7 findings, all judged real, still unticked): (1) the chain and the fold's capture start now begin at the live BOOT, as live builds both before any socket is dialled, not at the day's first frame: `--boot-ist HH:MM:SS` gives it, and without it the scheduled 08:30 boot is assumed (or the first frame when earlier); a first frame after 09:10 with no `--boot-ist`, or a frame before a given `--boot-ist`, reads incomplete (exit 2); the summary prints where the chain started (`BootInstant`, `chain_start_ist_ns`, `frames_before_boot`); a box booted at 09:14:40 with its first frame at 09:15:01.2 now decides the opening bar, where the old replay refused it as `chain_start` and still read complete; (2) the isolation guard reads `std` and `tokio` as allowlisted roots (files, I/O, paths, collections, clocks, the runtime) and bans a process spawn or a socket under any root (`Command::new`, `TcpStream`, `UdpSocket`, `net::`, `hyper`, `ureq`, `aws_smithy` and the like), and rule 1 and rule 4 strip raw identifiers, so `live_fill::r#replay` is the same path; (3) under `tickvault_trading` and `live_fill_report` the allowlist names exact items (`fixture::FIXTURE_COMMIT`, the engine's traits and types, the recipe types, the report's pure builders), so `write_demo_fixture`, `ReferenceDouble`, `rows::read_day` and `recheck::read_seconds` are refused; (4) `replay_tests.rs` starts with `#![cfg(test)]`, so the production panic guard skips it; (5) `replay_full_day_cost_harness` is on the ignored-test allowlist with its reason (31 to 32); (6) an empty path for any path flag is refused at parse (exit 2), as `--out ""` wrote into the current folder; (7) the lot-size check reports a derivative the artifact does not hold, or holds with no lot, as a problem (exit 2), never a silent pass. Each fix has a test that failed with the fix reverted (the guard's three by reverting each in turn). The golden now passes `--boot-ist 08:30:00`; its only changes are the signal ids (derived from the chain start), the events' order that follows them, and two summary lines; decisions and fills are unchanged.
  - 2026-11-13 (LF-8 review round 5, 9 findings, all judged real (findings 2 and 7 are the same defect), still unticked): (1) the replay's fold is handed over as live hands it over at every lane start (`set_replay_mode(true)` then `finish_replay` on the empty fold, as `LiveIngest::finish_wal_replay` does after the always-run WAL refold), so its capture start is provisional and the first folded tick confirms it at its own receipt; before, it stayed at the boot, so a first trade between the boot and the first receipt read as a new trade (an extra decision live never made); (2, 7) a loss found before the day's chain exists is held and judged by its next readable frame: on the previous date, or at or before the chain's start, it is counted apart (`losses_before_chain`), and after the chain's start (damage at the head of the day's first segment past the boot) it is handed to the chain as it is built, so the 09:15 bar is refused `tick_dropped` instead of decided untainted; (3) the isolation guard reads `self::super::..` paths against the `super` allowlist (they passed unread); (4) rule 1 expands `super::{..}` and `self::{..}` groups inside `live_fill/` (`use super::{chain, replay};` passed); (5) the LF-7 report reads the process start from the chain start, not the first frame, so a pre-market boot no longer reads Amber `process_started_after_0900`; (6) an unzipped `.sha-` copy in a local folder is refused like a zipped one (exit 3); (8) the lot-size cross-check reads only artifact rows expiring on or after the day and refuses an id held on two such rows with different lots (never first-wins); (9) a chosen `.gz` with no `--staging` is refused (exit 2) before `--out` is made or any segment is read (it was exit 4 after earlier segments replayed). Each fix has a test that failed with the fix reverted (the guard's two by reverting each in turn). The golden's only changes: the report now reads complete/green (an 08:30 boot), the summary's report light, two relabelled summary lines and the report file's hash; decisions and events unchanged. The 09:10 incomplete line is kept as a conservative bound (the boot now matters only to the chain's 09:15 `chain_start` taint).
  - 2026-11-13 (LF-8 review round 6, 6 findings, all judged real, still unticked): (1) the replay folds only what the live drain folds (`frame_feeds_the_candle_fold`): a TrueData record or a v1 to v3 order-update record (no endpoint byte, so it reads as main feed) is counted (`other_transport_frames`, a new summary line) and skipped before the clock, the receipt checks and the packet walk; before, it was walked as Dhan binary, abandoned and counted as a loss, refusing later candles `tick_dropped` once TrueData is on (latent: it ships off), and a legacy order-update record with no receipt failed the day; (2) the isolation guard refuses a `#[path]` attribute (also in `cfg_attr`) or an `include!`, `include_str!` or `include_bytes!` whose path (escapes decoded, a `concat!` split joined) has a component named `replay`, in any live module (rule 1) and in `main.rs` (rule 4); (3) an incomplete day is not settled, so the LF-7 report files read partial and Amber with `unsettled` (they said complete and Green while summary.txt said INCOMPLETE); (4) the non-trading-day, non-empty `--out` and missing-config refusals are tested on a folder that would replay (each case also run with only its condition removed, exit 0), and `--allow-non-trading-day` is covered; (5) the bucket go-ahead (second copy exit 3, no segment of the day exit 2, the download cap exit 2 unless `--yes`) is one pure function, `bucket_go_ahead`, unit-tested at the cap edge and for its order; (6) the 1 s bar memory figure is restated as about 80 B per kept second, about 3.5 to 4 GB for 2,000 liquid contracts over a full day (Assumed from the types, not measured; it said about 2 GB). Each fix has a test that failed with the fix reverted (the guard's by reverting the new check). The golden's only changes: the report reads partial/amber (its day ends at 09:21), the summary's report light, the new skipped-records line and the report file's hash.
  - 2026-11-13 (LF-9 hardening, 12 confirmed findings of the LF-6b/LF-7/LF-8 review, 8 and 9 the same defect, still unticked): (1) a held order's early finish counts only volume its pricing allows: the chain settles each second's share once the next second is known and a second beyond the LIMIT counts nothing (`allot_second`, shared with the re-pricing); (2) a held exit reserves its quantity (`exit_already_held`) and the runtime reports what it booked for every Execute back through a pre-allocated ring, so the chain's position follows the paper book (`PositionReport`, `on_position_report`, `forget_unsent`); (3) held orders of one instrument share each second's 25% cap, earlier orders first, and the daily re-pricing and the offline replay walk the held orders the same way (`recheck_walk`; an order overlapping one the chain finished unpriced reads `shared_volume_unknown`); (4) a Mark no longer triggers the runtime's O(orders) republish scan; (5) the paper command channel is a pre-allocated ring, the thread's counter publish resolves its handles once, and `dhat_live_fill` now drives older trades, late ticks, every taint kind with held orders, backlogged passes and held orders through finish (rows per order only); (6) the report marks a day only after the summary row that says its copy is verified; (7) a catch-up of a day with no local state asks the bucket and the summary row first and never rebuilds a day already copied; (8, 9, 10) the chain's bars take the candle fold's own per-tick volume (opening trade, `u32` wrap, counter restart); (11) a main-feed socket that stays down counts one loss per change of the down mask, never one a second, so a parked socket no longer taints every instrument for the rest of the day; (12) after a day roll the chain decides nothing until the process restarts (`reload_for_day` exists for a later reload and has no production caller; corrected by the LF-9 review below). Each fix has a test that failed with the fix reverted. Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (LF-9 review, 12 findings judged against the code, all real; 4, 5 and 11 one defect, 6 and 12 one defect; still unticked): (1, HIGH) a chain bar's close moves on every trade the fold does not call stale (`ConsumeStats::stale_packet`, carried on `TapTick`, 64 B unchanged), so an index printing twice in one second closes where `candles_1m` closes (LF-9 had kept every `older_trade` off the close); (2) an exit decided while its entry is still held is sized against the position the entry opens (`held_entry_qty`), not refused `exit_without_position`; (3) an order already known to send nothing (taint, late price) takes no share of later settled seconds; a share taken before that was known is not given back and later orders read `shared_volume_unknown`, as before; (4, 5, 11) a late 1 s print that moves a settled fill second (or the settled second it is the candidate of) past the LIMIT refuses the order (`skip:late_fill_price`), fail-closed, instead of pricing it short or over the shared cap; (6, 12) `reload_for_day` is documented as having no production caller, the stale-setup warning now says the chain waits for a restart, and the CLAUDE.md row is corrected; (7) a catch-up whose bucket probe fails builds a never-reported day locally without copying or marking it, and a complete local folder is probed before its copy; (8) a day found in the bucket is marked only when its summary row already says the copy was verified; (9) tests now drive the runtime's give-up and late-booked exit reports and the thread's own report and forward steps (`apply_position_reports`, `forward_outputs`); (10) volume of tapped ticks the tap ring dropped is carried to the instrument's next trade, so the chain's day total equals the fold's. Each fix has a test that failed with the fix reverted. Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (LF-9 review round 2, 11 findings judged against the code; 6 and 10 one defect; finding 1 judged wrong and not changed; still unticked): (1, NOT changed) with a book, shares past the best quantity are priced one more tick and cut at the LIMIT but still taken out of the order's remainder, so finishing at the trigger gives exactly the whole window's fill; a test pins it (`test_trigger_finish_equals_the_whole_window_with_a_book_past_best_qty`); (2, 7) the exit reservation (`held_reduce_only_qty`, `exit_already_held`) is removed: the held book finishes an exit only after every earlier held order of its instrument, and lists ready orders in insert order, so the runtime books the entry or the earlier exit first and clamps the exit against what is left (a short first exit is completed by the second, an exit in a lower slot never reaches the runtime before its entry); (3) volume of dropped tapped ticks is added to the next trade only inside a tainted stretch, else discarded (`missed_volume_discarded`) and the instrument's day volume marked unknown; (9) an instrument whose first tapped tick follows a loss the same day without the fold crediting its whole cumulative is marked unknown too; an unknown instrument's orders are refused `day_volume_unknown` for the day (refuses more than needed when the loss was upstream of the fold, never fewer); (4) the re-pricing treats a known-share order whose re-price differs from the chain's own model price as share-unknown for the later overlapping orders (`shared_volume_unknown`, not a false mismatch); (5) a stale-setup day roll sets `tv_live_fill_engine_configured` to 0 from the thread's own pass step (`CounterPublisher::note_inert`, local `/metrics` only); (6, 10) `read_row_uploaded` keeps "no row" apart from "read failed", and a catch-up whose bucket probe failed never builds a day whose summary row could not be read (`probe_failure_may_build`, reported `summary_row_read`); (8) one `TapVolume::from_stats` mapping shared by the drain, the replay and the tests, with a drain-level test against a twin fold; (11) a source scan pins the runtime's republish inside `if changes_orders {`. Each fix has a test that failed with the fix reverted (finding 1: an evidence test, no fix). Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (LF-9 review round 3, 11 findings judged against the code, all real; 3, 5 and 8 one defect, 2 and 9 one defect, 6 and 11 one gate; still unticked): (1, HIGH) the day-long `day_volume_unknown` mark refuses only entries, so a paper position opened before the mark is still closed by the strategy's exit (`test_regression_a_day_volume_unknown_mark_still_lets_an_exit_out`); (2, 9) `PendingBook::on_taint` bounds a triggered order at its trigger's candidate second, so an exit waiting for an earlier held order is not refused by a taint in seconds it never used (`test_regression_a_triggered_waiting_exit_ignores_a_later_taint_on_taint`); (3, 5, 8) the first-tick unknown mark skips the fold's own seed (`ConsumeStats::seeded` carried as `TapTick::fold_seed`): a mid-session boot's first trade and a cash equity's 09:15 first trade after the auction are no longer refused for the day after an unrelated loss, while a dropped seed is still caught (`test_regression_a_seed_tick_after_a_loss_is_never_marked_unknown`, driven through the real fold); (10) a counter restart on a dropped tap tick is carried to the next trade as a restart and marks the day unknown (`test_regression_a_dropped_counter_restart_tick_never_lengthens_a_chain_bar`); (4) the re-pricing walk keeps a held order whose Decision row was dropped, with its share unknown (`test_regression_a_held_order_missing_its_decision_row_leaves_later_orders_unknown`); (7) a day folder counts as finished only with its summary row's own local record (`upload::ROW_WRITTEN_RECORD`), so the old-day pass rewrites a copied day's missing row from its report file (`test_unreported_day_folders_finds_an_old_unmarked_day_and_folder_copied_clears_it`); (6, 11) `dhat_live_fill` drives the dropped-volume carry, discard and unknown mark in (C) and counts held-order refusals per reason and late fill prices from the chain's Reject rows in (D); its taint minute moved from 10 to 8 of each twelve, because the minute before 10 was always a drained-behind minute whose stretch refused the order at decision, so no held order was ever tainted. Each fix has a test that failed with the fix reverted (the DHAT gate failed with clock-step taints ignored on held orders, and with the discard branch removed). Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (LF-9 review round 4, 7 findings judged against the code, all real; 6 and 7 documentation and test honesty; still unticked): (1) the re-pricing walk also treats a known-share order whose rows carry the chain's `late_fill_ticks=N` (N above 0) as share-unknown for the later overlapping orders: a late print that only adds volume to a settled second moves the order's re-priced share and trigger with its price unchanged, which the price comparison missed (`test_regression_a_late_volume_amendment_with_equal_price_leaves_later_orders_unknown`; the key is one constant, `live_fill::LATE_FILL_TICKS_KEY`, the chain's reason is built from it); honest limit, recorded at the site: a late print after the order finished, or on an order the runtime then refused, is still not seen; (2) `widen_late` refuses a held order only when the late print moves a settled second that gave it shares (`Slot::gave`, pre-sized), so a late print on a zero-share second (floored to 0, left empty by earlier orders, or at the horizon) no longer refuses a valid order (`test_regression_late_price_on_a_zero_share_settled_second_refuses_nothing`); (5) the first tapped tick that ended a replay gap (`ConsumeStats::gap_reseed`, carried as `TapTick::fold_gap_reseed`, the struct still 64 B) is exempt from the `day_volume_unknown` first-tick mark like the seed, so a mid-session boot that re-folded the WAL tail no longer refuses that instrument's entries for the day after an unrelated loss (`test_regression_a_replay_gap_tick_after_a_loss_is_never_marked_unknown`, through the real fold's replay and hand-over; the chain.rs comment that claimed every mid-session first trade is a seed is corrected); (3, 4) the CLAUDE.md rows for the tap and the chain are corrected (`TapTick` 64 B and a 4 MiB ring, six new `ConsumeStats` fields, `IngestOutcome::Folded` carrying `TapVolume` on every folded tick; the DHAT count re-measured as 3,505 order-free steps and 95 order steps, worst 8 blocks per order; the held book's O(64²) per-pass worst case); (6) the `fill_window` module header now describes the shared, LIMIT-filtered trigger and the real per-pass cost; (7) the first-sight test now checks that a non-trade packet adds no volume (renamed `test_first_sight_packet_opens_no_bar_and_adds_no_volume`), and the two overlapping-exit tests no longer assert against the removed `exit_already_held` reason (they assert no Reject for the held exits). Each code fix has a test that failed with the fix reverted. Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (LF-9 hardening round, 6 findings judged against the code, all real; still unticked): (1, 4) a late 1 s print on a second the chain never released a bar for is that second's price once it is final: kept for a second whose successor turns out final with no bar, it is the candidate of the second before it when `settle_key` settles that one (`late_widen_bar`); arriving after that second settled, or after a later second was appended, it is inserted as a settled second with no volume (`insert_late_second`), so `price_held_window` reads it and an order whose settled share it moves past the LIMIT is refused `skip:late_fill_price`; kept prices for seconds an append has passed are dropped (`take_late_widen`), so they no longer fill the four `LATE_WIDEN_SLOTS` (`test_regression_late_print_on_an_untraded_second_after_a_settled_share`, `test_regression_kept_late_price_is_the_candidate_when_its_second_never_trades`, `test_regression_stale_kept_late_prices_do_not_crowd_out_a_waiting_one`); honest limit: the inserted second has no volume where the stored candle has the late trade's, so the re-pricing can still give it a share (the order carries `late_fill_ticks`); no chain-vs-re-pricing test for that case was built; (2) `dhat_live_fill` reads the paper command ring in production mode: (D) is never busy-polled, (E1) sends 2,000 Marks each waking a consumer task parked in `recv()` on another thread, (E2) fills a ring nobody drains to its 64 slots and past them (`send_output`'s full branch); a temporary allocation in `try_send`'s wake branch failed (D) and (E) (14,800 blocks), where the old (E) passed it; the CLAUDE.md row is corrected; (3) the old-day pass lists never-copied folders first and counts a slot only for a run that made progress; a folder run once with no progress is not run again by the process (`walk_old_days`, `test_regression_stuck_old_days_never_starve_a_never_copied_one`); (5) a `u32` wrap of the cumulative on a tapped tick after a global loss is read by the fold's own wrap rule (`forward_cumulative`), not as a dropped restart (`test_regression_u32_wrap_after_a_global_loss_is_not_a_dropped_restart`); (6) `ChainSetup::trading_day` is the day the setup was checked for and the chain's setup day comes from it, never from the start clock, so a setup checked before midnight on a chain started after it stays inert (`test_regression_a_setup_checked_before_midnight_is_never_todays_setup`). Each fix has a test that failed with the fix reverted. Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (LF-9 final independent check, still unticked): fmt, clippy (common, storage, trading, core, app, all targets, `-D warnings -W clippy::perf`), the named lib filters, the app and common guard tests and the three hooks (through a temporary index) were run on the uncommitted change. One gate failed: `source_scan_budget_guard` refused the new `include_str!` scan in `order_runtime_live_fill.rs` (review round 2, finding 11) because the file had no budget row; the row is added at 1 with its paired behaviour test (`test_handle_live_fill_cmd_asks_for_the_republish_only_after_an_execute`) named, and the guard failed before the row and passes after it. Findings 1 and 11 of the original twelve were re-proven by reverting the fix briefly (their tests failed, then the fix was restored byte for byte). Paper only: no order path, broker, page, alarm, EMF name, IAM, terraform, `[live_fill] enabled` or `select_engine` change.
  - 2026-11-13 (audit B2, sockets, order safety and ops; built, not merged, stays unticked until it merges; from the 2026-11-13 production audit findings 6 to 11): (6) the order-update dial is bounded by the main feed's `DIAL_TIMEOUT` (15 s) through `dial_order_update_socket` in `crates/core/src/websocket/order_update_connection.rs`; a dial that never finishes is a `Connect` error and takes the ordinary reconnect and paging path (test `test_regression_order_update_dial_times_out_on_a_silent_peer`, failed with the bound removed); (7) every Dhan WebSocket TCP socket (main feed, depth, order update) is dialled by `crates/core/src/websocket/ws_dial.rs::dial_ws_tls_with_keepalive`, which opens a `tokio::net::TcpSocket` with `SO_KEEPALIVE` (and `TCP_NODELAY` when asked) before the connect and hands it to `tokio_tungstenite::client_async_tls_with_config`; TLS, config and the read path unchanged; the sysctl file still sets the timings (tests read the option back: `test_tcp_connect_with_keepalive_sets_so_keepalive`, `test_dial_ws_tls_with_keepalive_upgrades_a_plain_socket`; guards `ws_nagle_disabled_guard.rs` and `websocket_connect_config_guard.rs` moved to the new call and pin the helper); (8) the three new-order exit live arms (`place_super_order`, `place_forever_oco`, `place_order_sliced` in `crates/trading/src/oms/engine.rs`) call `check_live_exit_gates` after the dry-run branch and before the token fetch: the sandbox sentinel (`tv_sandbox_gate_blocks_total`, existing) and `check_live_order_gates` (halt latch, readiness); `dry_run` unchanged, no setter added, `[exit_orders]` unchanged, cancels still exempt; not done: routing them through the `*_with_policy` wrappers (STOP-ALL latch, DH-904 ladder, DH-901 rotate-retry, halt engagement on their errors), which needs wrapper variants for three more endpoints; (9) `reset_daily` resets the order budget and `clear_order_halt` no longer does; (10) the hourly hard-stop guard, in its already-stopped branch, disables the `daily-start` rule while this month's budget-stop latch holds (`hold_current_month_budget_stop`, `crates/aws-lambdas/src/hard_stop_guard.rs`; existing DisableRule grant, no new IAM, no page; fail-open on an unreadable latch); honest limits: up to one hour between the latch write and the disable (a crossing reported in the hour before 08:30 IST can still start the box), and a crossing of the code line below the native $90 limit writes no latch and is still caught only once the box runs; (11) `.github/workflows/grow-ebs-volume.yml` defaults `target_size_gb` to 150 and refuses above 150 (Quote 51), IOPS and throughput unchanged (`aws_deploy_safety_guard.rs::grow_ebs_workflow_defaults_to_150_and_refuses_a_bigger_disk`); `variables.tf` validation ceiling (600) not changed here.
  - 2026-11-13 (audit B2 review round 1, 9 findings, 7 distinct (two pairs were the same defect), all judged real, built, still unticked): (a) a live gate refusing after `circuit_breaker.check()` left the single HalfOpen probe taken with no success or failure recorded, so every later order on every path read CircuitBreakerOpen until the 16:00 reset (dormant: `dry_run` is hardcoded): `OrderCircuitBreaker::release_probe` (O(1), only while HalfOpen) is called on every refusal of `check_live_order_gates` (place, modify, the exit arms) and on both sandbox refusals (test `test_regression_live_gate_refusal_gives_back_the_half_open_probe`, failed with the release a no-op; source pin on both sandbox branches); (b) `modify_super_order_leg`'s live arm now calls `check_live_exit_gates(OrderEndpoint::Modify, ..)` (sandbox, halt, readiness) before its token fetch; the gate takes an endpoint (test `test_regression_super_leg_modify_takes_the_live_gates`, failed without the call; the source pin covers the fourth arm); cancels still exempt, `dry_run` untouched; (c) `OrderBudget::reset_daily` clears the DAY tier only: clearing the minute and hour tiers at 16:00 let a burst across the reset send twice the broker's 250/min or 1,000/hr in one window (tests `test_regression_reset_daily_keeps_a_full_minute_tier`, `the_daily_reset_clears_only_the_day_tier`, both failed with all tiers cleared); (d) the kill-switch page's step 3 now names `aws ssm delete-parameter` for the latch AND `aws events enable-rule --name tv-<env>-daily-start` (the hold keeps that rule off, and a deleted latch is never released), text only (test `test_regression_recovery_steps_name_the_latch_and_the_start_rule`); (e) the hold trusts the latch only when this guard's own month-to-date read (`mtd_usd`, the free Budgets API it already uses) is at or above this month's effective line; unknown or lower leaves the rule alone (test `test_regression_hold_needs_this_months_spend_over_the_line`; wiring guard pins latch, spend read, compare, disable in that order); (f) the hard-stop guard cron moves 0 -> 50 (`budget-guards.tf`), so its 02:50 UTC run holds the rule before the 03:00 UTC start (two rules in the same minute have no order); residual window: a latch written 02:50 to 03:00 UTC can still let that start through, then the 03:50 UTC run breach-stops it at 09:20 IST if spend is over; side effects: out-of-window runs now land at XX:20 IST (a box running at 08:20 IST without keep-alive is stopped, per the window policy; the after-close catch moves 17:30 -> 17:20); takes effect only at the next Terraform apply (pin `hard_stop_guard_runs_before_the_daily_start_in_the_same_hour`); (g) `scripts/aws-upgrade-instance.sh --ebs-size` (was 10 to 200) and the `variables.tf` `ebs_gp3_size_gb` validation (was 10 to 600) both stop at 150 like the workflow, and its stale description is corrected (the grow-workflow test now also reads the script and the validation; same line count, shell budget green). Not run: the aws-lambdas crate build and tests (3.3 GB free, below the 6 GB floor; CI covers it), so (d) and (e) are Assumed until CI runs them.
  - 2026-11-13 (audit B2 review round 2, 7 findings, 5 distinct (two pairs were the same defect), all judged real, built, still unticked): (a, f) the HalfOpen probe is also given back on every other exit after `circuit_breaker.check()` that records no outcome: a failed token fetch (`probe_back_on_token_err`, wrapped around the fetch at place, modify, cancel and the five exit arms; `reconcile` and `verify_order_execution` take no probe), a throttled exit-arm answer (`record_exit_api_failure`), and a broker class the breaker does not count on place, modify and cancel unless the policy retries (`apply_engine_error_policy`, now a wrapper over `decide_engine_action`); tests `test_regression_failed_token_fetch_gives_back_the_half_open_probe`, `test_regression_rate_limited_exit_probe_gives_back_the_half_open_probe`, `test_regression_rate_limited_place_probe_gives_back_the_half_open_probe`, `test_token_fetches_after_the_breaker_check_go_through_the_probe_release`, each failed with its fix removed; (b) `modify_order` now runs the sandbox sentinel (`refuse_inside_sandbox`, shared with the exit gate) before halt and readiness; only a gate was added, `dry_run` untouched; (c) the hard-stop guard's minute-50 run at 08:20 IST no longer stops a box inside the curfew's 08:00-08:30 operating slot (budget breach still stops), tests `test_pre_open_slot_leaves_an_early_started_box_alone`, `test_pre_open_slot_still_takes_a_budget_breach_stop`, `test_before_the_curfew_open_still_stops`, and the stale `cron(0 ...)` row in the full daily-universe file now reads minute 50; (d) `grow-ebs-volume.yml`, `aws-upgrade-instance.sh` and the Terraform validations hold IOPS at 3000 and throughput at 125 (Quotes 30, 32/33), pinned by `grow_ebs_workflow_defaults_to_150_and_refuses_a_bigger_disk` (failed against the 16000 ceiling); (e) the order budget's day tier is the IST calendar date and nothing clears it at the 16:00 reset (`OrderBudget::reset_daily` removed), so a day's 7,000 can no longer be spent twice in one IST date; tests `the_day_tier_is_the_ist_calendar_date` and `test_regression_reset_daily_keeps_the_day_tier_of_the_ist_date` failed with the old rolling window (⚠ CORRECTED in round 5: this note first said all four tests did; `a_backward_clock_step_never_reopens_the_day` passes under the rolling window and pins only the `>` comparison, failing if the roll uses `!=`, and `ist_day_start_is_ist_midnight` is a pure helper test that never calls `would_admit`); (g) the latch test now runs with spend over the line so only the latch check keeps the rule on. Not run here: the aws-lambdas crate (disk under 6 GB; CI covers it).
  - 2026-11-13 (audit B2 review round 3, 4 findings, all judged real, built, still unticked): (1) `crates/core/tests/websocket_connect_config_guard.rs` now refuses ANY library dial (`connect_async*`, `client_async*`, a call, an import or a function value) outside `crates/core/src/websocket/ws_dial.rs`, explicit config or not, so a new socket module cannot dial with SO_KEEPALIVE off; the keepalive dial still needs an explicit config; bite: the old guard's own test accepted `connect_async_tls_with_config(request, Some(..), ..)`, the new `the_guard_flags_the_library_dial_outside_the_dial_module_even_with_a_config` refuses it. Honest limit: a source scan, so a dial built another way (a raw socket wrapped by `WebSocketStream::from_raw_socket`) is not seen. (2) `ws_dial::tcp_connect_with_keepalive` gives each resolved address `PER_ADDRESS_CONNECT_TIMEOUT` (5 s, compile-time checked to fit twice in `DIAL_TIMEOUT` 15 s) through the generic walk `connect_first_reachable`, so a first address that drops SYNs no longer uses up the whole dial (`test_a_silent_first_address_does_not_use_up_the_dial`, paused time; fails with the per-address limit removed). (3) every live local refusal after `order_budget.try_consume` (sandbox, halt, readiness, the per-second limiter, the circuit breaker; place, modify, cancel, verify and the exit arms) gives the order back (`OrderBudget::give_back`, `OrderManagementSystem::release_unsent_order`, `rate_and_circuit_or_give_back`), so refusals during a halt no longer spend the hour tier and lock out cancels after `clear_order_halt` (`test_regression_halt_refused_orders_spend_no_budget`, `..._readiness_refused_modify_spends_no_budget`, `..._circuit_refused_orders_spend_no_budget`, all three fail with the give-backs removed); dry-run ordering unchanged; `dry_run` stays hardcoded. (4) the budget is per IST date WITHIN ONE PROCESS: a restart starts a fresh count (doc in `rate_limiter.rs` and the CLAUDE.md `oms/` row corrected; stale `clear_order_halt` comment fixed). FOLLOW-UP, not built: keep the day count across restarts (for example rebuilt at boot from the day's `order_audit` rows); must land before live orders are switched on. Not reachable while `dry_run` is hardcoded.
  - 2026-11-13 (audit B2 review round 4, 2 findings (the same defect twice), judged real, built, still unticked): two local refusals after `order_budget.try_consume` kept one order of budget spent with nothing sent. (1) A failed token fetch: `probe_back_on_token_err` now takes `nothing_sent` and gives the budget back as well as the HalfOpen probe; the five exit arms pass `true`, the place, modify and cancel loops pass `!auth_retry_used` (after an auth rotate-retry the first request was sent and stays counted), and `verify_order_execution` uses `budget_back_on_token_err` (no probe). (2) The DATA-805 STOP-ALL latch: place, modify and cancel check it once before the first send (`refuse_unsent_inside_stop_all`), which gives back the probe and the budget and raises the stop-all alert once per episode as the policy arm does; a latch the call's own ladder engages later keeps the budget spent (counts more, never less). Tests `test_regression_failed_token_fetch_spends_no_budget`, `test_regression_stop_all_latch_refusals_spend_no_budget` and `test_token_fetch_failing_on_the_auth_retry_pass_keeps_the_budget` each failed with its fix reverted; the source pin `test_token_fetches_after_the_breaker_check_go_through_the_probe_release` counts the 3 + 5 + 1 wrapped fetches and the 3 latch checks. The `clear_order_halt` comment and the CLAUDE.md `oms/` row now name the two paths that still count. Only refusals and give-backs added; `dry_run` stays hardcoded, no setter, `[exit_orders]` unchanged; not reachable while `dry_run` is hardcoded. `cargo test -p tickvault-trading --lib oms::`: 900 passed; clippy `-D warnings` clean; fmt clean.
  - 2026-11-13 (audit B2 review round 5, 4 findings, 3 distinct (two were the same dial defect), all judged real, built, still unticked): (1) `ws_dial::connect_first_reachable` gives the per-address 5 s limit only to an address with another one still to try; the last or only address runs until the caller's `DIAL_TIMEOUT` (15 s), as the library dial did, so a single-address host whose first SYNs are lost still connects on the kernel's 7 s retransmit; tests `test_the_only_address_may_use_the_whole_dial` and `test_the_last_address_after_a_silent_one_is_not_cut_short` (both failed with `TimedOut` with the cap put back on every address), `test_the_address_walk_reports_timeouts_and_skips_refusals` rewritten (all addresses silent is bounded only by the outer dial timeout; silent then refused reports the refusal after 5 s); (2) the hard-stop guard's pre-open exemption (step 1c) now applies only to a box launched on this IST date (`launched_on_this_ist_date`; an unknown launch time is not exempt), so a box the drain hold kept up from yesterday whose 07:35 IST curfew stop failed is still stopped by the 08:20 IST run; test `test_regression_pre_open_slot_stops_a_box_held_up_from_yesterday` (written to fail with the old 1c, which took the `pre_open` arm for any running box; NOT compiled or run here, see below), plus `test_launched_on_this_ist_date_uses_the_ist_calendar`; NOT the review's suggested "at or after 07:30 IST" bound, which would stop the 07:10 IST manual start 1c exists for (`test_pre_open_slot_leaves_an_early_started_box_alone`); honest limit: a box launched after 00:00 IST today and held until 07:29 is still exempt; the stale `market_open_readiness.rs` comment about the guard's 03:00 UTC tick now reads minute 50; (3) the round 2 note's revert evidence is corrected in place (only two of the four tests fail under the rolling window), and `a_backward_step_after_ist_midnight_counts_on_the_newer_date` is added, which fails under both the rolling window and a `!=` roll (both mutations run here: the rolling window failed only the new test, confirming the finding; `!=` failed it and the older backward test). Not run here: the aws-lambdas crate (disk stayed at 2 to 2.5 GB free for 30 minutes, under the 6 GB floor; CI covers it), so the two hard-stop guard tests above are unverified locally.
  - 2026-11-13 (audit B2 final independent check, still unticked): fmt, clippy `-D warnings` on core, trading and storage (all targets), the trading lib (2073 passed), core lib (2948 passed), the exit-order lockout guard (14), both websocket guards (5 + 5), storage `aws_deploy_safety_guard` (17) and the common shell budget, rust-only, browser surface, codebase map and deploy safety guards all green, each binary checked to embed this worktree's path; one text fix: the `ebs_gp3_iops` and `ebs_gp3_throughput` descriptions in `variables.tf` still said "Range 3000-16000" and "Range 125-1000" after the validation was cut to 3000 and 125, now they say so. Honest limit found on the way: worktrees sharing one target folder can reuse each other's test binaries (dep-info paths are relative), so an earlier run here first executed another worktree's trading binary; the runs above were redone after touching each crate's sources. Not run: the aws-lambdas crate (2.2 GB free, under the 6 GB floor), so the hard-stop guard and kill-switch tests are verified only by CI.
