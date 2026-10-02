# Handoff — PR #2004 (zero-loss audit fixes)

> Purpose: let a cold Claude session (any account) resume this work from the
> repo alone. Updated after every milestone. Last updated: 2026-10-02 17:15 UTC.

## Where things are

| Item | Value |
|---|---|
| PR | https://github.com/SJParthi/tickvault/pull/2004 — DRAFT, owner merges (never merge or arm auto-merge) |
| Branch | `claude/project-thread-x09qc8` |
| Last pushed head | `bd81c0a98` — CI fully green (All Green success), no conflict with main |
| Plan file | `.claude/plans/active-plan-audit-fixes-2026-09-26.md` (round items R4-1..R4-6 ticked) |
| Comparison page | https://claude.ai/artifact/ACvpJxrDNpHwi2GB1NNmuX (version 4; publish new versions to the SAME link) |
| Owner | Parthi. Wants verified claims only, plain-language tables, O(1) hot paths, Rust only (except frontend), zero tick/data loss |

## Done

- Rounds 1–4 of audit fixes are in the PR (see the PR body and the plan file).
- Round 4 = depth 805 self-recovery, backup copy of top 1,000 contracts on a
  spare main-feed socket, in-place resubscribe, reader core pinning (libc,
  owner-approved), HOT-PATH-STALL-01 page, plus the 8 attack-pass fixes in
  `bd81c0a98`.
- Round 5 verification (2026-10-02): Rust-only guard 29/29, shell budget guard
  5/5, zero `.py` files, banned-pattern and O(1) scanners clean on all 78
  changed `.rs` files, `dhat_rescue_floor_zero_alloc` and `dhat_multi_tf_fold`
  pass. `dhat_ws_lag` / `dhat_live_ingest_seam` not run locally (disk) — CI
  runs them.

## In progress — round 5 fixes (NOT yet committed when this file was written)

If the working tree is gone, redo these from the descriptions below.

| ID | Severity | Where | Fix |
|---|---|---|---|
| F1 | medium | `crates/app/src/main_feed_backup.rs` (`write_backup_set`, `ReplayBackup`), `dhan_feed_stack.rs` (`load_replay_backup`, `refold_wal_frames`) | Persist a bounded HISTORY of backup-set publications; replay picks the latest publication at or before each frame's receipt time, same IST day. Today one file is overwritten per publish, so a crash-then-republish leaves older WAL frames undeduplicated (duplicate rows). |
| F2 | low | `main_feed_backup.rs` `publish_backup_set` / `BackupDedup::adopt` | Carry dedup slot state over for contracts that stay in the set (a republish currently resets it, so in-flight second copies are accepted again). Replay must mirror it. |
| F3 | low | `crates/core/src/websocket/pool_supervisor.rs` `overflow_episode_note_disconnect` | Publish `OVERFLOW_WIDEN_PERMITTED` only under the lock; the step must see the 805 (no stale "yes"). |
| S1 | medium | `crates/storage/src/seal_writer_task.rs` `drain_recovered_seals` / `BootWritten` | Older-copy guard must survive a halted boot drain (persist a bounded summary beside `archive/`, seed the next boot, count refusals). |
| S2 | low | `crates/storage/src/raw_frame_upload.rs` `write_marker` | On a full disk the marker write fails after a verified upload, so prune refuses forever. Keep a bounded in-process "verified uploaded" record; retry the marker. Never delete without verified copy. |
| S3 | low | `crates/storage/src/ws_frame_spill.rs` `seed_frame_seq_from_disk` | Seed the frame sequence from max(disk segments, persisted applied watermark + 1). |
| L | limit | CLAUDE.md rows | Record: replay dedup can keep both copies when they straddle the applied-watermark skip or one was ring-shed (duplicate, never loss); `*.bin.N` / set-aside files are never uploaded or pruned. |

## Next steps (exact)

1. `cd` to the worktree, `git status`; if round-5 edits exist, review them.
2. Tests: `cargo test -p tickvault-app main_feed_backup`, `cargo test -p tickvault-core pool_supervisor`,
   targeted `cargo test -p tickvault-storage <module>`; `cargo clippy --workspace --no-deps -- -D warnings`;
   `cargo fmt --all --check`; `bash .claude/hooks/banned-pattern-scanner.sh`; plan-gate.
3. Add `R5` items to the plan file (ticked, with test names), commit with `-F file`
   (body cites §), push to `claude/project-thread-x09qc8`.
4. Wait for CI (PR activity subscription); fix any red; keep the PR a draft.
5. Update the PR body (Round 5 section) and publish comparison page version 5 to the same link.
6. Update this file.

## Open decisions (owner only)

- Merging PR #2004 — owner's call.
- D11 (Muhurat trading capture, Sunday 2026-11-08): waiting for Parthi's session date, hours
  and cost. Not started; local branch `wip/d11` has no commits beyond this PR branch.

## Ground rules for the resuming session

- Draft PR only; never merge, never arm auto-merge, never force-push, never `--no-verify`.
- bruteX repo is READ-ONLY from here.
- Every claim labelled Verified / Assumed with real output.
- Keep usage lean: 2–3 helper agents, targeted tests, let CI run the full matrix.
