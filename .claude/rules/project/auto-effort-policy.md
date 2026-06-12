# Auto-Effort Policy — Claude self-scales effort per task (operator lock 2026-06-12)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file > defaults.
> **Scope:** PERMANENT. Every task, sub-task, PR, sub-PR, every current and future
> Claude Code / Cowork session in this repo.
> **Operator approval (2026-06-12, verbatim intent):** *"from now onwards can you
> take this automation effort dude?"* — granted standing permission for Claude to
> auto-scale its own effort without being told each time.
> **Auto-load:** this file lives under `.claude/rules/project/`, so it is read at the
> start of every session.

---

## The rule (one line)

**Claude judges each unit of work and auto-scales effort: deep + multi-agent on
risky/complex work, fast + solo on trivial work — without asking the operator to
set it each time.**

---

## Honest capability boundary (NO hallucination)

| Wish | Reality |
|---|---|
| Claude flips the harness **`Effort` slider** (Faster ⇄ Smarter / Ultracode) itself | ❌ **Cannot** — that is a user/UI control; **no tool or hook is exposed to the model to move it**. |
| Claude scales the **actual work effort** per task (depth of reasoning + whether to spawn parallel sub-agents) | ✅ **Can** — this is what this policy governs. |

This policy delivers the *outcome* (effort matches the task automatically) via
Claude's own judgment + dynamic sub-agent fan-out — NOT by touching the slider.
The operator keeps the slider wherever they like; Claude self-throttles the work
so max capability is never wasted on a trivial change (token-aware, consistent
with the `aws-budget.md` cost discipline).

---

## The effort ladder (mechanical judgment)

| Tier | When Claude applies it | What it means |
|---|---|---|
| ⚡ **Light** | typos, single-line edits, doc/comment tweaks, quick factual questions, status checks | fast, solo, no agent fan-out |
| 🔧 **Standard** | a focused change in one crate, a small bug fix, a single-file feature | normal reasoning, scoped tests, no full agent panel |
| 🚀 **Heavy** | multi-crate or >1,000 LoC changes, hot-path / OMS / risk / auth / persist code, anything "must not break," **every PR before merge** | deep reasoning + adversarial 3-agent review (hot-path + security + hostile) per `operator-charter-forever.md` §E + `wave-4-shared-preamble.md` §3 |

**Token-aware guardrail:** never burn Heavy effort on Light work. The 3-agent
panel is reserved for the Heavy tier (the charter already mandates it there).

---

## Interaction with existing rules

- This policy does NOT relax any mechanical gate. The design-first wall, plan
  enforcement, serial-PR protocol, banned-pattern scanner, and the per-item
  guarantee matrix still apply at full strength regardless of effort tier.
- The Heavy-tier 3-agent review is the SAME requirement already in
  `operator-charter-forever.md` §E — this file just makes "when to escalate"
  an automatic judgment instead of a per-request ask.
- The `ultracode` / `ultrathink` input keywords (operator-typed) still force a
  given turn to max effort; this policy is the *automatic* default in their
  absence.

---

## Auto-driver explanation

> Sir, you told the head chef once: "you decide how much effort each dish needs —
> don't ask me every time." So for a glass of water 💧 he works fast and alone; for
> the wedding feast 🎉 he thinks deeply and calls his 3 inspectors. He never wastes
> the big effort on small dishes, and he never cuts corners on the big ones. One
> permission, forever.

---

## Trigger (auto-loaded paths)

Always loaded. Reinforced on any session that opens this repo, edits
`.claude/rules/`, or starts any task/PR.
