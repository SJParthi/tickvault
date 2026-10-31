# Handoff: plan leftovers (W4 / M6 / EX1 / S1 / DT-small), 2026-10-31

Branch `handoff/plan-leftovers`. No PR opened. This was the drafted PR body; the M6 proposal below is not built.

## Summary

This PR covers the five open items in `.claude/plans/active-plan-audit-2026-10-20.md` (owner, 30 Oct 2026).

| Item | Status | Evidence |
|---|---|---|
| W4: parked live acks under latched apply lag | **Built here** | `wal_applied_watermark.rs::{ProbeVerdict, ParkedConfirm, confirm_parked_if_due, parked_confirm_span}`, `wal_unapplied_ranges.rs::UnappliedRanges::remove_span`, `wal_suspension_watcher.rs::{probe_verdict, WalLagTracker::take_forgot_untrusted}`. Storage lib: 2260 passed, 0 failed, 1 ignored (Verified, local). |
| EX1: daily expiry-scope check | Already on main (#2134, #2135); ticked | Every test the plan names exists on main (Verified). The app tests run in CI on this PR; they were not re-run locally because the session disk was full. |
| S1: silence detector out of slots | Already on main (#2134); ticked | `TickGapDetector::observe_tracked`. The two core tests passed locally (Verified); the app test runs in CI. |
| DT-small: three causes of a frame waiting | Already on main (#2134, #2135); ticked | `queue_seed_batch`, `step_idle_work`, `with_fold_capacity`, `row_buffer_presize_bytes`. All named tests exist (Verified); they run in CI. |
| M6: evening restart replays up to 5× the frame cap | **Not done; proposal below** | The cap is a constant in `crates/storage/src/ws_frame_spill.rs`, which another session in this wave owns. |

### What W4 changes

- **Before.** While QuestDB has a table latched untrusted for apply lag, every probe is unclean and live acks park. At the end of the episode the whole parked stretch is marked unapplied. That WAL then stays pinned until a later replay reads it, which is about one session of WAL per lag-latched day.
- **Now.** The watcher reports a `ProbeVerdict`:
  - `Clean`
  - `LagOnly`: the only fault is a table latched untrusted for apply lag
  - `Unsafe`: a suspended table, an unparsed row, a blind probe, or a table the tracker dropped while it was still untrusted
- **The new rule.** An episode that only ever saw `LagOnly` leaves a pending record beside its exit range. The record is confirmed at a later clean probe once three things are true. These are the replay settle step's rules:
  - 60 s (`WATERMARK_DURABILITY_LAG_SECS`) has passed since the exit;
  - two more clean probes have reported;
  - no new episode has begun.
- **What confirmation does.** It removes the span above the episode's entry window from the unapplied ranges, and raises each sink's watermark to its highest parked ack.
- **Never confirmed:**
  - an `Unsafe` episode;
  - the entry window (acks taken before the episode was seen);
  - a span that any other range was added over (`held_contaminated`).
- **Cost.** It runs on the watcher task only: O(32) plus one sidecar write and one persist per confirmation. Nothing runs per tick or per ack.
- **Counter.** `tv_wal_parked_confirm_total{outcome}`, local `/metrics` only. There is no filter, no EMF name and no alarm.

**Live measurement asked for by the plan: Unknown.** This session had no AWS CLI and the logs connection failed, so `tv_wal_unapplied_pinned_bytes`, `tv_wal_suspect_seconds_total` and `tv_wal_suspect_episodes_total` were not read. Whether any session has had clean probes after an episode is unmeasured. If none has, this confirms nothing, and the probe rule is not relaxed to make it.

**Limits:**
- A restart drops the waiting record; the range stays, so more is kept.
- An episode still open at the stop, or a latch that flaps on and off, confirms nothing.
- A lag-only latch is trusted as "acked into QuestDB's WAL, applied once the lag clears". That is the same trust the settle step gives a replayed batch, so a later `RESUME WAL FROM` a later transaction would still lose those rows, as it would any acked row.

### Hostile review

Three lenses (correctness, O(1)/allocation honesty, edge cases), every finding attacked by a refuter, max effort, 11 agents.

| Finding | Verdict | Action |
|---|---|---|
| HIGH: an empty or partial reply from a restarting QuestDB evicts an untrusted table, which reads as `Clean` and could confirm a stretch that is still unapplied | Survived | **Fixed:** `WalLagTracker` records forgetting an untrusted table on all three removal paths; that probe is `Unsafe` (`test_forgetting_an_untrusted_table_is_reported_on_every_removal_path`) |
| MEDIUM: the same thing through a table held only by `watermark_hold` | Survived (same cause) | Fixed by the same change |
| LOW: `probe_verdict` took over `emit_wal_lag`'s doc comment and TEST-EXEMPT marker | Survived | **Fixed:** moved above it |
| MEDIUM: confirmation fires on latch release while the backlog is still deep | Refuted (the same trust every ack gets) | none |
| LOW: off_worker runs on every clean probe; held_full repeats; the property test checks safety only | Refuted | none |

A second review round has not run yet (see below).

### M6 proposal (for the session that owns `ws_frame_spill.rs`)

1. In `crates/storage/src/ws_frame_spill.rs`:
   - Add `static WAL_REPLAY_FRAME_CAP: AtomicU64 = AtomicU64::new(WAL_REPLAY_MAX_FRAMES_PER_BOOT)`.
   - Add `pub const EVENING_REPLAY_FRAME_CAP_MULTIPLIER: u64 = 5`.
   - Add `pub fn set_wal_replay_frame_cap_for_boot(cap: u64) -> u64`. It clamps `cap` to `[WAL_REPLAY_MAX_FRAMES_PER_BOOT, 5 × WAL_REPLAY_MAX_FRAMES_PER_BOOT]`, stores it, and returns it.
   - `replay_all_with_report_at` takes the cap as a parameter. `replay_all_with_report_fenced` passes `WAL_REPLAY_FRAME_CAP.load(Acquire)`. The refusal log line prints the cap actually used.
2. In `crates/app/src/evening_drain.rs`, add a pure `frame_cap_for(&EveningState, standard, now_unix, secs_ist) -> u64`. It applies the same freshness and window rule as `catchup_budget_for` and returns `standard × 5` only for a fresh pending evening restart. Add `peek_frame_cap(standard) -> u64`, which reads the state file **without clearing** the pending flag (`budget_or` clears it later).
3. In `crates/app/src/main.rs`, before the STAGE-C boot replay, call `set_wal_replay_frame_cap_for_boot(evening_drain::peek_frame_cap(WAL_REPLAY_MAX_FRAMES_PER_BOOT))`. A morning boot, a boot with an unreadable state file or a stale pending flag keeps 3,000,000. The disk floor, the 15-minute clock, the 3 restarts and the 40-minute spacing are unchanged.
4. Tests (named in the plan): `test_regression_evening_restart_gets_five_times_the_frame_cap`, `test_morning_boot_keeps_the_frame_cap`, `test_unreadable_state_keeps_the_frame_cap`, plus one for the clamp.

## Needs the owner

- Nothing in this PR needs a fresh dated quote: no new page, alarm, EMF name, Terraform, budget, instance or disk change, and nothing switched on.
- M6 needs the session that owns `ws_frame_spill.rs`, or a decision to lift that ownership for this change.

## Testing Done

- `cargo test -p tickvault-storage --lib`: 2260 passed, 0 failed, 1 ignored.
- Storage integration tests that touch the changed code: `dhat_rescue_floor_zero_alloc` 1, `error_level_meta_guard` 7, `questdb_commit_mode_doc_guard` 1, `wal_seal_pubfn_coverage_guard` 2, `wal_suspension_properties` 10, `wal_unapplied_ranges_properties` 8. All passed.
- `cargo test -p tickvault-core --lib observe_tracked`: 2 passed.
- `cargo clippy -p tickvault-storage -- -D warnings -W clippy::perf`: clean. With `--all-targets`, the only errors are in test code I did not touch (`partition_archive.rs`, `seal_dlq.rs`, `seal_writer_task.rs`, `spill_ceiling_is_free_space_based_guard.rs`).
- `banned-pattern-scanner.sh`, `pub-fn-test-guard.sh`, `plan-gate.sh`: pass.
- Not run locally: app crate tests (disk). CI runs them.

## Zero-Loss Guarantee Charter

- **No new loss path.** Every doubt keeps the range: an unsafe episode, a forgotten table, contamination, a 33rd range, a restart, or a new episode.
- **O(1) honesty.** The new work is O(32) on the cold watcher task. There is no per-tick or per-ack work and no hot-path allocation.
- **Evidence.** The test counts above are pasted from real runs. The live measurement is stated as not done.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01MdGCF4EdAFDEv2E2yrgKtC
