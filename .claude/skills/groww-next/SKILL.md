---
name: groww-next
description: Drive the next serial sub-PR of the Groww second-feed plan end-to-end (sync → review → implement → gate → PR → auto-merge → monitor).
disable-model-invocation: true
allowed-tools: Bash, Read, Edit, Write, Grep, Glob, Agent
---

Drive the **next** Groww second-feed sub-PR to merge, one PR at a time, per the
serial-PR protocol (`pr-completion-protocol.md`) and the operator charter. Do NOT
start a second PR until the current one merges.

Authoritative plan: `.claude/plans/active-plan-groww-second-feed.md` +
`.claude/rules/project/groww-second-feed-scope-2026-06-19.md` (the operator lock).

## Steps

1. **Sync + ground.** `git fetch origin main` → merge into the working branch →
   read the plan file to find the next unchecked sub-PR. Confirm the operator
   lock (§32 = Python validation path; native Rust = production option).

2. **Design-first.** Ensure `.claude/plans/active-plan-groww-second-feed.md` has
   the 6 sections (Design/Edge Cases/Failure Modes/Test Plan/Rollback/
   Observability), Status APPROVED, referencing the crate(s) this sub-PR touches.

3. **Adversarial review (if >3 crates or >1,000 LoC, or hot-path/auth/persist).**
   Spawn 3 subagents in parallel — `hot-path-reviewer`, `security-reviewer`,
   `general-purpose` (hostile) — BEFORE coding. Fix every CRITICAL/HIGH inline.

4. **Implement** the next sub-PR. Honour the locks: 2 Dhan WS unchanged; Groww
   default-OFF + isolated `groww_*` tables; capture-at-receipt durability;
   composite `(security_id, segment)` uniqueness + DEDUP keys; no `feed` in a
   live Dhan dedup key (it is a provenance label).

5. **Gate locally:** `cargo test -p <crate>` + `cargo fmt --check` +
   `cargo clippy -p <crate> --lib` + `bash .claude/hooks/banned-pattern-scanner.sh`
   + `bash .claude/hooks/pub-fn-test-guard.sh "$PWD" all` +
   `bash .claude/hooks/plan-gate.sh "$PWD"`. All must pass.

6. **Commit + push** (`git push -u origin <branch>`, retry on network error).

7. **Open PR as draft** with the 15+7 guarantee matrix + honest-100% envelope →
   mark **ready** → **enable auto-merge (squash)** → **subscribe to PR activity**.

8. **Monitor to merge.** On CI failure: read the job log, diagnose, fix, push —
   auto-merge re-fires on green. On merge: tick the plan item, unsubscribe.

9. **Repeat** for the next sub-PR only AFTER the current one is merged.

## Honest envelope
The **Python `growwapi` sidecar** sub-PRs cannot be run/verified in a cloud
sandbox (no `growwapi`/network) — build them correct-by-construction vs
`docs/groww-ref/` and flag that they need operator verification on the Mac. Never
report an unrunnable sidecar as "tested".
