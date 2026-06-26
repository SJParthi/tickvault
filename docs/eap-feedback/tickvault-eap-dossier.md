# Claude Code EAP Feedback Dossier — TickVault

**Purpose:** Living, field-collected feedback from real Claude Code sessions on the TickVault codebase, structured for the Claude Code Early Access Program team. To be consolidated with a BruteX dossier into one submission to claude-code-early-access@anthropic.com.

**Last updated:** 2026-06-26

> **NOTE — this is an APPEND-ONLY / living document.** It is organized by the EAP team's 5 feedback topics. New findings are appended under the relevant topic over time; existing entries are not rewritten. Each finding carries an ID, severity, a UNIVERSAL vs PROJECT-SPECIFIC tag, concrete reproducible evidence from a real session, and a suggested fix.

**Legend — Severity:** High / Medium / Low (problems) · WIN (positive observations).
**Legend — Tag:** UNIVERSAL (applies to most/all Claude Code users) · PROJECT-SPECIFIC (specific to this repo's setup; universal lesson noted inline).

---

## 1. Use-case fit

### FIT-01 — Strong fit for adversarial-review + mechanical-gate workflows
- **Severity:** WIN
- **Tag:** PROJECT-SPECIFIC (the pattern is UNIVERSAL — agentic verification pairs naturally with ratchet-test/gate-driven repos).
- **Evidence:** Claude Code fits TickVault's adversarial-review + mechanical-gate workflow well. This session ran the charter-mandated 3-agent review (hot-path + security + hostile bug-hunt) entirely via subagent fan-out against a real PR, used the GitHub MCP for the full PR lifecycle (create → ready → auto-merge → subscribe), and used background tasks for monitor-to-merge. The repo's ratchet-test/gate design (banned-pattern scanners, DEDUP-key meta-guards, error-code cross-ref tests, etc.) pairs naturally with agentic verification — the gates give the agent objective, machine-checkable targets.
- **Suggested fix:** None — capture as a validated success pattern. Consider documenting "gate-driven repo + agentic verification" as a recommended EAP use-case template.

---

## 2. Reliability — autonomy-to-completion

### REL-01 — Background subagents + task-notifications are reliable for autonomy
- **Severity:** WIN
- **Tag:** UNIVERSAL
- **Evidence:** 4+ parallel background workers were launched in this session. Each completed and automatically re-prompted the orchestrator with its result, enabling a clean fan-out / fan-in without any polling by the orchestrator. The completion-notification mechanism was dependable across all workers.
- **Suggested fix:** None — capture as a validated reliability strength.

### REL-02 — Remote git clone is shallow/grafted with a stale `origin/main`, breaking diff-based review
- **Severity:** High
- **Tag:** UNIVERSAL
- **Evidence:** The remote execution environment provides a SHALLOW, grafted git clone whose `origin/main` points at a stale tip. As a result, `git diff origin/main...HEAD` fails with "no merge base", and a naive `git diff origin/main HEAD` is polluted with ~149 files / ~18K lines of already-merged noise. A verification worker had to manually discover the true base SHA (the PR's parent commit) to recover the real 7-file delta; relying on `origin/main` would have produced a wrong / over-broad review.
- **Suggested fix:** Provision full git history (or a correct merge-base) in remote sessions, or expose the PR base SHA to agents explicitly (e.g., as an environment variable or session metadata) so diff-based review targets the correct base.

### REL-03 — Pinned toolchain components can be missing/broken in the remote container
- **Severity:** Medium
- **Tag:** UNIVERSAL
- **Evidence:** `cargo fmt --check` failed with "rustfmt ... not applicable to the '1.95.0' toolchain" — the rustfmt component could not be fetched through the proxy. The format gate therefore could not be verified locally; only CI covers it, reducing local pre-merge confidence. **Confirmed 2026-06-26** — the local pre-push gate `.claude/hooks/pre-push-gate.sh [1/8] cargo fmt --check` HARD-FAILS on the missing rustfmt component and blocked even a DOCS-ONLY commit (zero .rs files changed; the [8/8] gate confirmed no .rs files). The gate has no "unrunnable component" detection, so a broken toolchain component is indistinguishable from a real formatting violation, blocking unrelated commits.
- **Suggested fix:** Ensure pinned toolchain components (e.g., rustfmt, clippy) are present in the remote image so local pre-merge gates can run without depending on outbound component fetches. Additionally, the pre-push fmt gate should detect a missing/unrunnable rustfmt component and skip-with-warning rather than hard-fail, and/or the container image must ship the pinned rustfmt+clippy components.

---

## 3. Memory

### MEM-01 — Persistent plan/task files drift out of sync with merged work
- **Severity:** Medium
- **Tag:** PROJECT-SPECIFIC (universal lesson = cross-session task-state that reconciles with reality).
- **Evidence:** Multiple APPROVED plan files (`live-feed-health-sp5/sp6`, `one-candle-engine`) show 0 of N checkboxes ticked even though their corresponding PRs already merged into `main`. There is no automatic reconciliation between merged PRs and the persistent plan checklist, so the on-disk task-state diverges from the actual state of the repository.
- **Suggested fix:** Provide a mechanism to reconcile persistent task-state against merged PRs / git history at session start, so plan checklists reflect what has actually landed.

### MEM-02 — Very large always-on context with no relevance gating
- **Severity:** Low-Medium
- **Tag:** PROJECT-SPECIFIC (universal lesson = lazy / relevance-based rule loading).
- **Evidence:** CLAUDE.md plus ~40 `.claude/rules/` files auto-load every session — tens of thousands of tokens are consumed before any task begins. The content is comprehensive but heavy, and there is no obvious lazy-by-relevance loading; most rules are irrelevant to any given task yet are always present.
- **Suggested fix:** Support relevance-gated or on-demand rule loading so the always-on context stays lean, loading rule files only when the active task touches their trigger paths.

---

## 4. Proactivity

### PRO-01 — Parallel orchestration matched the operator's "parallelise everything" intent
- **Severity:** WIN
- **Tag:** UNIVERSAL
- **Evidence:** Independent reviewers (hot-path / security / hostile) and a report-builder ran concurrently as background agents, cutting wall-clock time. All were launched in single fan-out batches and reconciled on completion — proactively parallelizing independent work without being told to micro-manage each step.
- **Suggested fix:** None — capture as a validated proactivity strength.

### PRO-02 — Orchestrator lacked direct read/grep, forcing a subagent for trivial lookups
- **Severity:** Medium
- **Tag:** PROJECT-SPECIFIC config (universal lesson = right-size the orchestrator's toolset).
- **Evidence:** In this session the orchestrator / main agent had NO direct Read / Bash / Grep — every file read or shell command required spawning a subagent (~20-60s each). This is excellent for large fan-out, but it adds latency for trivial one-file lookups where a single direct read would suffice.
- **Suggested fix:** Give the orchestrator lightweight read/grep directly, reserving subagents for heavier isolated work, so trivial lookups don't incur subagent spin-up latency.

---

## 5. Session-UX

### UX-01 — Web-driven Claude Code requires all user-facing communication via `mcp__webagent__reply`; plain text is never shown
- **Severity:** Medium
- **Tag:** UNIVERSAL
- **Evidence:** Web-driven Claude Code requires ALL user-facing communication to go through the `mcp__webagent__reply` tool; plain assistant text is NEVER shown to the user. A Stop hook (`stop-hook-reply-gate.py`) blocked turn-end after the assistant wrote a normal-text reply that never reached the user, forcing a re-send via `mcp__webagent__reply`. The friction: it is easy to believe you've answered the user when you actually have not, because the assistant's plain text looks like a normal reply but is silently dropped.
- **Suggested fix:** Surface this constraint earlier / more visibly (e.g., at session start or in the system context), or auto-route a turn's final assistant text through `reply` when no reply tool was called during the turn.

### UX-02 — Stop-hook reminder text is self-contradictory / mis-templated (references absent slackbot tools)
- **Severity:** Medium
- **Tag:** UNIVERSAL
- **Evidence:** The Stop-hook reminder text is self-contradictory / mis-templated. It tells the agent to "Call `mcp__slackbot__reply` / `mcp__slackbot__react` / `mcp__slackbot__no_reply_needed`", while the actually-available tools are `mcp__webagent__*` and NO slackbot tools exist in the session. The verbatim hook output in this session mixed both integrations in one message, which is confusing and points the agent at a non-present tool set.
- **Suggested fix:** Template the hook per the active integration (webagent vs slackbot) so it never references a tool set that is not present in the session.

---

## How to reproduce / methodology

These findings were collected during a single live Claude Code session on the TickVault repository on 2026-06-26, while running the project's charter-mandated PR workflow (adversarial 3-agent review + GitHub MCP PR lifecycle + background monitor-to-merge). Each entry's **Evidence** describes what was directly observed in that session:

- **Git/toolchain findings (REL-02, REL-03)** reproduce by inspecting the remote clone (`git log`, attempting `git diff origin/main...HEAD`, and running `cargo fmt --check`) in the remote container.
- **Memory findings (MEM-01, MEM-02)** reproduce by listing `.claude/rules/` + reading the APPROVED plan files and comparing their checkbox state against merged PRs on `main`.
- **Session-UX findings (UX-01, UX-02)** reproduce by ending a turn with plain assistant text in a web-driven session and observing the Stop-hook (`stop-hook-reply-gate.py`) output verbatim.
- **WINs (REL-01, PRO-01, FIT-01)** reproduce by launching multiple background subagents and observing automatic completion re-prompts and the fan-out/fan-in reconciliation.

## Open questions for the EAP team

_(placeholder — to be filled as the dossier matures and as the BruteX dossier is consolidated)_
