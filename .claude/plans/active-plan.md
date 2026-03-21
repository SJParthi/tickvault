# Implementation Plan: MCP Servers + Hooks + Agents Upgrade

**Status:** DRAFT
**Date:** 2026-03-21
**Approved by:** pending

## Context

Upgrade Claude Code integration with MCP servers for structured infrastructure
access, new hooks for failure handling, and new agents for security/dependency
auditing. Goal: improve accuracy and quality without compromising existing standards.

## Plan Items

- [ ] Configure MCP servers in `.mcp.json`
  - Files: `.mcp.json`
  - GitHub MCP server — structured PR/issue/check access
  - PostgreSQL MCP server — query QuestDB directly (port 8812)
  - Docker MCP server — container management for 8 services

- [ ] Add new hooks to `.claude/settings.json`
  - Files: `.claude/settings.json`
  - `PostToolUseFailure` — log failed tools with context for debugging
  - `Notification` — route notifications to structured log
  - Enhance `PostToolUse` matcher for Edit/Write to auto-check Rust files

- [ ] Create hook scripts for new events
  - Files: `.claude/hooks/post-tool-failure-log.sh`, `.claude/hooks/notification-log.sh`
  - PostToolUseFailure: extract tool name + error, log to stderr
  - Notification: log notification type + message

- [ ] Create `.claude/settings.local.json.template`
  - Files: `.claude/settings.local.json.template`
  - Template with placeholder secrets (GITHUB_TOKEN, GRAFANA_API_KEY)
  - Already in .gitignore (line 37 covers settings.local.json)

- [ ] Add new specialized agents
  - Files: `.claude/agents/security-reviewer.md`, `.claude/agents/dependency-checker.md`
  - security-reviewer: OWASP, injection, secret exposure, unsafe blocks
  - dependency-checker: Bible version compliance, audit, deny

- [ ] Document local vs cloud workflow for heavy CI
  - Files: `docs/architecture/local-vs-cloud-workflow.md`
  - Mutation testing (cargo-mutants): LOCAL ONLY — generates 50GB+ artifacts
  - Coverage (cargo-llvm-cov): LOCAL ONLY — doubles build size
  - Normal dev/test: CLOUD is fine (29GB free, target/ is 795MB)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | GitHub MCP configured | `mcp__github__*` tools available for PRs/issues |
| 2 | QuestDB MCP configured | SQL queries via MCP, no raw docker exec |
| 3 | Docker MCP configured | Container health/logs via structured tools |
| 4 | Tool fails (e.g. Bash timeout) | PostToolUseFailure logs context to stderr |
| 5 | Notification sent | Notification hook logs to stderr |
| 6 | New dev joins | settings.local.json.template shows what secrets to set |
| 7 | Security review needed | security-reviewer agent scans for OWASP issues |
| 8 | Dependency update | dependency-checker verifies Bible compliance |
