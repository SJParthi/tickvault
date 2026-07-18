# Rules Diet — 2026-07-18 (path-triggered frontmatter conversion)

> **Dated context (2026-07-18):** the operator runs many parallel Claude
> sessions on this repo; measured worker-agent failures traced to
> context overflow — the always-loaded `.claude/rules/` set had grown to
> ~1,462,393 bytes (~1.43 MB), and spawned workers autocompact-thrashed.
> The 2026-07-18 recon (coordinator-assigned) proved the load mechanism
> and this PR applied the coordinator-approved Option A (frontmatter in
> place, zero file moves).

## The mechanism (measured fact)

The Claude Code harness loads `.claude/rules/**/*.md` RECURSIVELY as
always-on context UNLESS a file carries YAML `paths:` frontmatter — then
it loads only when a matching path is being worked on. Prose "## Trigger"
sections are NOT machine-honored (empirically verified: all 15 dhan/
files + all 81 non-frontmatter project/ files were injected at session
start; the 11 frontmattered files were not). Directory placement is
irrelevant to loading.

## What changed (this PR)

- 56 runbook-class `project/` files + all 15 `dhan/` files gained
  `paths:` frontmatter derived from their own prose Trigger sections
  (Source: lines for the wave/phase runbooks). Zero moves, zero content
  deletions. `runbook_path()`, the crossref/tag/triage guard tests, the
  `error-rules.yaml` paths, and the tickvault-logs MCP runbook search
  are all untouched (path-stable).
- Always-loaded rules context: 1,462,393 B → ~505 KB (the 25 KEEP
  governance/lock files, 503,052 B, + this note).
- The 4 mega scope-locks (daily-universe, groww-second-feed, no-rest,
  ws-scope-lock; 279.6 KB combined) stay always-loaded — trimming them
  is a separate follow-up PROPOSAL requiring operator approval, not this PR.

## Honest degradation

Trigger entries of the form "any file containing X" (content-substring
triggers) cannot be encoded in `paths:` globs. Those files now load on
path match only; otherwise they remain reachable via the crossref test,
`mcp__tickvault-logs__find_runbook_for_code`, `runbook_path()`, and
manual Read. This trade-off was coordinator-approved 2026-07-18.
