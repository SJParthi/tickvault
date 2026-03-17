Show a one-glance status dashboard of the project.

## Steps

1. Run `.claude/hooks/session-sanity.sh` for the safety systems dashboard
2. Run `git log --oneline -5` to show recent commits
3. Run `git status --short` to show current working tree state
4. Read the current phase doc header from `docs/phases/phase-1-live-trading.md` (first 30 lines)
5. Summarize:
   - Current branch
   - Uncommitted changes count
   - Last 5 commits (one line each)
   - Phase progress (what's done, what's next)
   - Any warnings from the dashboard
