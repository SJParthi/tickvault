# tickvault handoff — 2026-10-06 (refreshed at each milestone)

Paste the block below into a new Claude Code session (any account) that has
`SJParthi/tickvault` attached.

```text
Repo: SJParthi/tickvault (Rust). You are continuing work from another Claude
session. First read CLAUDE.md, .claude/rules/project/ (auto-loaded), the audit
file docs/audit-2026-10-04.md, and this handoff: branch
claude/handoff-2026-10-06, folder docs/handoff/ (HANDOFF.md, *.patch, specs/).
Do NOT merge the handoff branch; it only carries files. Delete it when done.
The 51c patch now also contains its local commit; apply it on origin/main.

STANDING RULES (owner, verbatim spirit): label every claim Verified, Assumed or
Risk. Rust only. Hot paths O(1), never block, zero tick loss. Merge only on
All Green on the exact head SHA (ci.yml enable-auto-merge does the merge; never
merge by hand). Never force-push or use --no-verify. AWS is read-only (never
dispatch deploys; deploys run automatically on merge / after-close cron / the
08:30 IST box start). Total cost stays under 15,000 INR a month. One open PR at
a time (pr-completion-protocol). Rule-file-first for any rule change (dated
quote). At most 5 active plan files: add items to
.claude/plans/active-plan-feed-hardening.md (ITEM 51 post-market cross-verify,
ITEM 52 live-socket fast lane). Commit and push as separate commands; commit
messages with § via `git commit -F <unique file>`. Build with
`CARGO_INCREMENTAL=0 cargo ... --target-dir <repo>/target`; if disk is low,
delete old test executables in target/debug/deps (older than 60 min).
Owner approvals 2026-10-06: "Go ahead with whatever you want dude", "See do
everything whatever is recommended dude okay?", "fix and resoleve evryhtign
then emrge and dpeloy okay?".

ORDER OF WORK (serial, one PR open at a time):
1. PR #2032 MERGED 2026-10-06 18:07Z as ba5ffb0b4. Nothing to do; skip to 2.
2. 51c — post-market cross-verify: a failed fetch never hides a divergence;
   derived late window; new rule §12.15.9. Work in progress =
   docs/handoff/wt-51c.patch (base 145276dad). `git checkout -b
   claude/xverify-51c origin/main && git apply --3way docs/handoff/wt-51c.patch`
   (resolve conflicts against current main). Spec: specs/xverify-plan-judged.json
   prs[2], specs/xv-missing-minutes-page.md, specs/xv-read-too-early.md.
   Finish, run scoped tests (cargo test -p tickvault-app --lib --
   dhan_live_crossverify; common guards), hostile review until 2 complete clean
   rounds, draft PR, drive to merge.
3. Depth step 1 — length-framed binary spill tier for depth rows + replay
   (no table change, old text spill files must still replay). WIP =
   docs/handoff/wt-depth1.patch (base 750a45701), branch
   claude/depth-spill-framed. Plan: ITEM 49d/45i "Proposed sequence" step 1,
   add sub-item 49d-1 with Files/Tests and the six sections.
4. Audit H3 and M3 — WIP = docs/handoff/wt-h3.patch, wt-m3.patch (base
   145276dad), branches claude/audit-h3, claude/audit-m3. Read the findings in
   docs/audit-2026-10-04.md first; verify the patch matches them.
5. Remaining queue: post-market 51d–51j (specs/xverify-plan-judged.json
   prs[3..]); live-socket fixes 2–6 (specs/fl-*.md, fl-remaining.json:
   data-silent redial, 808 refresh, reader memory, live p99, log-drop
   narrowing); depth steps 2–4; audit H1 (~85 counters), M9, L6.
6. Needs prod access (new AWS read key): compare the next 09:15 ADANIENT
   candle volume with Dhan's chart (5 Oct: ours 51,051 vs Dhan 45.81K); run the
   depth array-row scratch test on prod QuestDB after 15:40 IST.

The patches are UNREVIEWED work in progress: treat them as a head start, not
as done. Every claim in them must be re-verified against current main.
Report status as a plain table (item, state, Verified/Assumed/Risk).
```

## State at snapshot (Verified unless marked)

| Item | State |
|---|---|
| #2022, #2026, #2028, #2029, #2030, #2031 | Merged. Deploy 10:11Z succeeded on 750a45701 (#2031 ships at next 08:30 IST start) |
| #2032 (52a) | MERGED 18:07Z as ba5ffb0b4 (All Green on 63a105f9f) |
| 51c | 2 local commits (a5c24ace9, 79ab81304) + fix-round edits; patch 9 files, +1,523/−133; review loop running (snapshot 18:53Z) |
| Depth step 1 | 1 local commit 59f396505 + fix-round edits; patch 10 files, +3,462/−42; review loop running (snapshot 18:53Z) |
| Audit H3 | Uncommitted WIP, 9 files, +483/−815 |
| Audit M3 | Uncommitted WIP, 12 files, +439/−7 |
| Unknown | Why deploys at 07:09Z and 09:46Z failed; whether #2022 terraform alarms are applied |
