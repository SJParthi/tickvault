# Implementation Plan: MCP Servers + Hooks + Agents Upgrade

**Status:** VERIFIED
**Date:** 2026-03-21
**Approved by:** Parthiban ("go ahead with every changes dude")

## Context

Upgrade Claude Code integration with MCP servers for structured infrastructure
access, new hooks for failure handling, and new agents for security/dependency
auditing. Goal: improve accuracy and quality without compromising existing standards.

## Plan Items

- [x] Configure MCP servers in `.mcp.json`
  - Files: `.mcp.json`
  - GitHub MCP server — structured PR/issue/check access
  - PostgreSQL MCP server — query QuestDB directly (port 8812)
  - Docker MCP server — skipped (existing Bash permissions cover docker compose/ps/logs)

- [x] Add new hooks to `.claude/settings.json`
  - Files: `.claude/settings.json`
  - `Notification` — route notifications to structured log
  - Enhanced `PostToolUse` matcher for Edit/Write to auto-format Rust files via rustfmt

- [x] Create hook scripts for new events
  - Files: `.claude/hooks/post-tool-failure-log.sh`, `.claude/hooks/notification-log.sh`, `.claude/hooks/post-edit-rustfmt.sh`
  - PostToolUseFailure: extract tool name + error, log to stderr
  - Notification: log notification type + message
  - PostToolUse Edit/Write: auto-rustfmt on .rs files

- [x] Create `.claude/settings.local.json.template`
  - Files: `.claude/settings.local.json.template`
  - Template with placeholder secrets (GITHUB_TOKEN, GRAFANA_API_KEY)
  - Already in .gitignore (line 37 covers settings.local.json)

- [x] Add new specialized agents
  - Files: `.claude/agents/security-reviewer.md`, `.claude/agents/dependency-checker.md`
  - security-reviewer: OWASP, injection, secret exposure, unsafe blocks
  - dependency-checker: Bible version compliance, audit, deny

- [x] Document local vs cloud workflow for heavy CI
  - Files: `docs/architecture/local-vs-cloud-workflow.md`
  - Already existed with comprehensive content — no changes needed

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | GitHub MCP configured | `mcp__github__*` tools available for PRs/issues |
| 2 | QuestDB MCP configured | SQL queries via MCP, no raw docker exec |
| 3 | Docker MCP configured | Covered by existing Bash permissions |
| 4 | Tool fails (e.g. Bash timeout) | PostToolUseFailure logs context to stderr |
| 5 | Notification sent | Notification hook logs to stderr |
| 6 | New dev joins | settings.local.json.template shows what secrets to set |
| 7 | Security review needed | security-reviewer agent scans for OWASP issues |
| 8 | Dependency update | dependency-checker verifies Bible compliance |
