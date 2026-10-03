# Handoff — PR #2004 (zero-loss audit fixes)

> Purpose: let a cold Claude session (any account) resume this work from the
> repo alone. Updated after every milestone. Last updated: 2026-10-03 04:00 UTC.

## Where things are

| Item | Value |
|---|---|
| PR | https://github.com/SJParthi/tickvault/pull/2004 — Parthi said "merge everything" (2026-10-02): merge it once All Green passes on the exact head |
| Branch | `claude/project-thread-x09qc8` |
| Last green head | `bd81c0a98` (All Green success); round 5 pushed after it |
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

## Round 5 fixes — committed on the PR branch (plan item R5)

All six are fixed with regression tests; see plan item R5 for test names.

| ID | Severity | Where | Fix |
|---|---|---|---|
| F1 | medium | `crates/app/src/main_feed_backup.rs` (`write_backup_set`, `ReplayBackup`), `dhan_feed_stack.rs` (`load_replay_backup`, `refold_wal_frames`) | Persist a bounded HISTORY of backup-set publications; replay picks the latest publication at or before each frame's receipt time, same IST day. Today one file is overwritten per publish, so a crash-then-republish leaves older WAL frames undeduplicated (duplicate rows). |
| F2 | low | `main_feed_backup.rs` `publish_backup_set` / `BackupDedup::adopt` | Carry dedup slot state over for contracts that stay in the set (a republish currently resets it, so in-flight second copies are accepted again). Replay must mirror it. |
| F3 | low | `crates/core/src/websocket/pool_supervisor.rs` `overflow_episode_note_disconnect` | Publish `OVERFLOW_WIDEN_PERMITTED` only under the lock; the step must see the 805 (no stale "yes"). |
| S1 | medium | `crates/storage/src/seal_writer_task.rs` `drain_recovered_seals` / `BootWritten` | Older-copy guard must survive a halted boot drain (persist a bounded summary beside `archive/`, seed the next boot, count refusals). |
| S2 | low | `crates/storage/src/raw_frame_upload.rs` `write_marker` | On a full disk the marker write fails after a verified upload, so prune refuses forever. Keep a bounded in-process "verified uploaded" record; retry the marker. Never delete without verified copy. |
| S3 | low | `crates/storage/src/ws_frame_spill.rs` `seed_frame_seq_from_disk` | Seed the frame sequence from max(disk segments, persisted applied watermark + 1). |
| L | limit | CLAUDE.md rows | Record: replay dedup can keep both copies when they straddle the applied-watermark skip or one was ring-shed (duplicate, never loss); `*.bin.N` / set-aside files are never uploaded or pruned. |

## Merge round: ONE combined PR (Parthi 2026-10-03 03:07 UTC: "fold all into a single pr and merge")

CI runners are scarce (a full run took ~9 h of queue), so every open PR that is finished is
folded into #2004 and merged with one CI run. Separate CI runs were cancelled (Parthi
approved pausing the other threads' checks, 03:07 UTC).

| PR | State at 2026-10-03 04:00 UTC | Next action |
|---|---|---|
| #2001 host tuning in Rust | MERGED (f55531ba9 on main) | none |
| #2003 cold bucket keeps everything | FOLDED into #2004 (main.tf: #2003's version; both guard sets kept) | close after #2004 merges |
| #2002 holiday gate in Rust | FOLDED into #2004 (no conflict) | close after #2004 merges |
| #2005 ensure-questdb in Rust | FOLDED into #2004 (this branch's earlier D6d copy was reverted first) | close after #2004 merges |
| #2006 crash marker (PR31b-1) | FOLDED; CLAUDE.md row added; loss-counter guard fix pushed here | close after #2004 merges |
| #2007 + #2008 audit rows (PR42a/b) | FOLDED (42b branch carries 42a) | close after #2004 merges |
| #2009 mid-session exit seal (PR31b-2b) | FOLDED; dhan_feed_stack conflict kept both methods. Its plan text is still not in the audit plan | close after #2004 merges |
| #2010 candle warm-up (PR31b-2a) | NOT folded: WIP part 1 of 2 | its thread finishes it, merges main |
| #2011 raw frames to S3 (45e) | NOT folded: #2004 already carries its own 45e-1 uploader (raw_frame_upload.rs); #2011 is a second, independent one (wal_raw_upload.rs). Keeping both = two uploaders | owner picks one; until then #2011 stays open |
| #1968-#1971 opentelemetry bumps | FOLDED as one combined upgrade (0.33 / tracing-opentelemetry 0.34) | close after #2004 merges |

Merges: squash, only with All Green success on the exact head. After each merge confirm the
PR's change is on main. Close a PR only if its content is verified already on main.

## Next steps (exact)

1. `git status` in the worktree; check PR #2004 CI on the latest head; fix any red.
2. Check #2003 and #2001 merged (list open PRs). Then merge origin/main into this branch,
   resolve overlaps keeping both sides, run targeted tests, push.
3. Do #2002 as in the table. Then the combined opentelemetry upgrade.
4. Update the PR body (Round 5 section) and publish comparison page version 5 to the same link.
5. Update this file and push.

## Open decisions (owner only)

- D11 (Muhurat trading capture, Sunday 2026-11-08): waiting for Parthi's session date, hours
  and cost. Not started; local branch `wip/d11` has no commits beyond this PR branch.

## Ground rules for the resuming session

- Merge only with All Green success on the exact head; never force-push, never `--no-verify`,
  never rebase someone else's branch (merge main into it instead).
- bruteX repo is READ-ONLY from here.
- Every claim labelled Verified / Assumed with real output.
- Keep usage lean: 2–3 helper agents, targeted tests, let CI run the full matrix.
- Build with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`; the disk allowance is small.
