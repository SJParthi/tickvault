# Claude Code Currency & Max-Effectiveness — every session (auto-loaded)

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** EVERY current + future Claude Code / Cowork session in this repo.
> **Why:** operator demand 2026-06-19 — *"without using the very new latest
> version … should be entirely used … in this current session and new sessions"*.
> **Auto-load:** this file lives under `.claude/rules/project/`, so it is read at
> the start of every session — zero per-session network cost (no changelog fetch
> on every boot; that is heavy + token-costly). Instead this rule **reminds** the
> session of the current high-value features + the canonical way to verify them.

---

## The honest boundary (no illusion)
- Claude cannot **upgrade** the CLI from inside a session — the running version is
  whatever the environment auto-updates to. Claude **uses** the features the
  running version exposes.
- Claude's training knowledge has a cutoff; **recent releases are invisible to
  memory**. The ONLY non-hallucinated way to know "what's newest" is to **fetch
  the changelog**, not recall it.

## The canonical way to verify the latest (do this when it matters)
When the operator asks about CC features, or before starting significant new
work, **WebFetch** `https://code.claude.com/docs/en/changelog.md` (or `/whats-new`)
— cite it, don't guess. (Verified 2026-06-19: latest was `2.1.183`.)

## The #1 effectiveness rule — RIGHT TOOL, RIGHT LAYER
| Need | Use |
|---|---|
| A reusable prompt template | **slash command** |
| Real domain logic + helper files | **skill** (`.claude/skills/`) |
| Isolated / parallel exploration | **subagent** (Agent tool) |
| Enforce a rule *with code* | **hook** (`.claude/hooks/`) |
| Repo constitution (keep lean) | **CLAUDE.md** |
| External system access | **MCP server** |
Anti-pattern: a behavioral rule written into a prompt instead of a hook; a
reusable workflow pasted into chat instead of a skill; an isolated task run in
the main context instead of a subagent.

## High-value features to USE by default (current as of 2026-06-19)
| Feature | When to use |
|---|---|
| **Parallel subagents** (hot-path + security + hostile) | every PR touching >3 crates or >1,000 LoC (operator-charter §E) — BEFORE and AFTER impl |
| **Nested subagents** (agents spawn agents, 5-deep) | deep multi-dimension research |
| **`/effort xhigh`** (Opus 4.8) | the hardest sub-PRs / risky hot-path / OMS / auth work |
| **`/goal`** | "keep working until the PR merges / the plan completes" |
| **PR-activity subscribe + auto-merge** | every PR — event-driven CI-fix + merge, never `sleep`-poll |
| **`/code-review --fix` + `/simplify`** | the quality pass before opening a PR |
| **Ratchet tests as guards** | every new invariant (O(1)/dedup/coverage) → a build-failing test |
| **`/groww-next` skill** | drive the next serial sub-PR of the Groww plan end-to-end |

## Mechanical enforcement note
This is a REMINDER rule (auto-loaded), not a build gate — the build gates that
actually enforce O(1)/coverage/security live in `.claude/hooks/` + CI per
`operator-charter-forever.md`. This rule keeps every session *aware* of the
current toolset so capability is never left on the table.

## Trigger (auto-loaded)
Always loaded. Reinforced on any session that asks about Claude Code features,
versions, automation, subagents, hooks, skills, or "use the latest".
