# Task T00 — EXAMPLE TEMPLATE (delete this comment when copying)

```yaml
ID: T00
Title: Example task — copy this file as tasks/T<NN>-<slug>.md to create a new one
Status: DRAFT
Owner:
ClaimedAt:
Branch: claude/task-T00-example
DependsOn: []
Touches:
  - <list of crates/files this task is allowed to edit>
Estimated: <Hh>
Severity-if-skipped: <CRITICAL | HIGH | MEDIUM | LOW>
Acceptance:
  - <observable, ratcheted criterion 1>
  - <criterion 2>
  - <criterion 3>
ReviewersRequired:
  - hot-path-reviewer (if touches hot path)
  - security-reviewer (if touches secrets or order placement)
  - general-purpose (adversarial bug-hunt, always required)
```

---

## Why this task exists (kid-friendly, 1 paragraph)

`<one paragraph explaining the WHY in kid-friendly auto-driver terms — what breaks today, what gets better after, why it matters for Day 1 of live trading>`

## What to do (step by step)

1. `<concrete step 1 — file:line precision>`
2. `<concrete step 2>`
3. ...

## What NOT to do (scope guardrails)

- DO NOT edit files outside `Touches:` list above.
- DO NOT merge to main from this task branch — operator does that.
- DO NOT skip the 3-agent adversarial review listed in `ReviewersRequired`.

## How to verify (per the 9-box checklist from per-wave-guarantee-matrix.md)

- [ ] 1. Typed `NotificationEvent` variant (if user-visible)
- [ ] 2. `ErrorCode` variant + runbook section in appropriate `wave-*-error-codes.md`
- [ ] 3. `tracing` macros use the new `code = ErrorCode::X.code_str()`
- [ ] 4. Prometheus counter/gauge added with static labels
- [ ] 5. Grafana panel renders the counter (wrapped in `increase()` / `rate()`)
- [ ] 6. Prometheus alert rule in `alerts.yml`, market-hours gated if applicable
- [ ] 7. Call site exists (`pub-fn-wiring-guard.sh` passes)
- [ ] 8. Triage YAML rule in `.claude/triage/error-rules.yaml`
- [ ] 9. Ratchet test in `crates/*/tests/` that fails build on regression

Plus reference the 15-row + 7-row guarantee matrix in
`per-wave-guarantee-matrix.md` in your PR description.

## How to claim this task (atomic)

```bash
# 1. Pull latest
git pull origin claude/trading-tick-vault-BkvpS

# 2. Confirm AVAILABLE
grep '^Status:' .claude/plans/friday-may-15-mega/tasks/T00-*.md
# should print: Status: AVAILABLE

# 3. Edit the yaml header above:
#    Status: CLAIMED
#    Owner: claude-sess-<your-unique-id>
#    ClaimedAt: 2026-MM-DDTHH:MM:SS+05:30

# 4. Commit + push IMMEDIATELY (atomic claim)
git add .claude/plans/friday-may-15-mega/tasks/T00-*.md
git commit -F /tmp/claim-msg.txt
git push origin claude/trading-tick-vault-BkvpS

# 5. If push rejected → someone beat you. git pull → re-check → abandon.

# 6. Create task branch
git checkout -b claude/task-T00-example
```

## When done

1. PR title: `T00: <short description>`
2. PR body MUST cite this task file path
3. PR labelled `task-completion`
4. Edit task file: `Status: REVIEW`
5. Update `_board.md` row
6. Wait for operator to merge
7. After merge → edit task: `Status: DONE`
