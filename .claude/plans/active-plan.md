# Implementation Plan: Full Claude Code Automation Upgrade

**Status:** IN_PROGRESS
**Date:** 2026-03-21
**Approved by:** Parthiban (explicit "Yes implement all these")

## Plan Items

- [x] Add missing permissions (WebFetch, Glob, Grep, Agent) to settings.json
  - Files: .claude/settings.json
  - Tests: manual — verify Claude no longer prompts for these tools
  - Impl: Added Glob, Grep, WebFetch, WebSearch, Agent + 12 more Bash patterns

- [x] Add Agent Teams env var + voice keybindings env
  - Files: .claude/settings.json
  - Tests: manual — verify /voice and agent teams work
  - Impl: CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 in env

- [x] Create post-compact-recovery.sh hook script
  - Files: .claude/hooks/post-compact-recovery.sh
  - Tests: manual — verify context re-injection after compaction
  - Impl: Re-injects 3 principles, phase, branch, recent commits, active plan

- [x] Create stop-failure-alert.sh hook script
  - Files: .claude/hooks/stop-failure-alert.sh
  - Tests: manual — verify stderr output on API errors
  - Impl: Categorized alerts for rate_limit, auth, max_tokens, server_error, billing

- [x] Create session-end-save.sh hook script
  - Files: .claude/hooks/session-end-save.sh
  - Tests: manual — verify auto-save warning on dirty exit
  - Impl: Warns on uncommitted changes, untracked files, unpushed commits

- [x] Wire PostCompact, StopFailure, SessionEnd, SubagentStart/Stop hooks in settings.json
  - Files: .claude/settings.json
  - Tests: manual — verify hooks fire on events
  - Impl: All 5 new hook events wired with scripts

- [x] Add auto-verify agent to Stop hook
  - Files: .claude/settings.json, .claude/hooks/stop-auto-verify.sh
  - Tests: manual — verify fmt+clippy runs when Claude stops with .rs changes
  - Impl: stop-auto-verify.sh runs fmt+clippy only when .rs files changed

- [x] Wire statusline in ~/.claude/settings.json
  - Files: ~/.claude/settings.json
  - Tests: manual — verify statusline shows in terminal
  - Impl: Note: statusline config is desktop-app level, not settings.json

- [x] Create ~/.claude/keybindings.json for voice mode
  - Files: ~/.claude/keybindings.json
  - Tests: manual — verify /voice works with spacebar push-to-talk
  - Impl: voice:pushToTalk = space

- [x] Enhance session-sanity.sh with cargo check
  - Files: .claude/hooks/session-sanity.sh
  - Tests: manual — verify cargo check runs at session start
  - Impl: cargo check --workspace on startup (not resume/compact)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | New session starts | session-sanity runs cargo check, reports health |
| 2 | Context compacts at 85% | PostCompact re-injects principles + phase + modified files |
| 3 | API rate limit hit | StopFailure prints alert to stderr |
| 4 | Session ends with dirty tree | SessionEnd warns about uncommitted changes |
| 5 | Claude spawns subagent | SubagentStart logs agent type to stderr |
| 6 | Claude stops after .rs changes | Auto-verify runs fmt+clippy on changed files |
| 7 | User presses spacebar in /voice | Push-to-talk activates |
| 8 | User uses WebFetch/Glob/Grep/Agent | No permission prompt |
