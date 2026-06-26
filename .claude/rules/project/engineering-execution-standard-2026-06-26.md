# Universal Engineering Execution, Validation, Merge, Hardening & Automation Standard (operator 2026-06-26)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > `zero-loss-guarantee-charter.md` > this file.
> **Scope:** PERMANENT, STANDING. Every PR, every sub-PR, every task, every sub-task, every current and future Claude Code / Cowork session in this repo.
> **Operator demand (2026-06-26, posted twice as a standing directive):** *every change is owned end-to-end through merge AND post-merge validation; every report is transparently labelled and evidence-backed; O(1) is judged honestly.*
> **Relationship to existing charters:** this file is a CONCISE delta. The detailed mechanical contracts (15-row + 7-row matrices, honest-100% wording, zero-loss envelope, 22-test taxonomy, automation-first, 3-agent review, serial-PR loop) live in `operator-charter-forever.md` + `zero-loss-guarantee-charter.md` — DEFER to them; this file only adds the genuinely-new emphases below.

---

## §1. PR ownership through merge + POST-MERGE validation (the operator's hardest demand)

The work is NOT done at "PR opened" or even "PR merged". For every PR:

1. **Review the complete implementation** against the charters before requesting merge.
2. **Drive it to merge** — per the 10-step serial loop in `pr-completion-protocol.md` / `operator-charter-forever.md` §H (one PR at a time).
3. **Confirm the merge** landed on `main` (`pull_request_read` → `state=closed, merged=true`).
4. **Validate POST-MERGE:** deploy success + runtime health + monitoring signals are green (`make doctor` 7-section, `mcp__tickvault-logs__run_doctor`, CloudWatch alarms clear, no novel error signatures, deploy/CI status green).
5. **ONLY THEN proceed to the next task.**

**Continuous monitoring (always-on, not one-shot):** watch build / test / review / security / deploy status throughout. The moment a conflict, failure, or regression appears — **immediately address it and re-merge**. Never leave a merged-but-unvalidated PR behind; never start the next task on top of an unhealthy merge.

---

## §2. Transparency taxonomy — mandatory labels in EVERY report

Never present assumptions as facts. Never hallucinate. Never claim verification without evidence. Tag every claim with exactly one label:

| Label | Meaning |
|---|---|
| **Verified** | Proven by concrete evidence (pasted test output, bench numbers, query result, log line) |
| **Assumed** | Believed but NOT yet verified — say so plainly |
| **Risk** | A potential issue that could materialize |
| **Blocker** | Prevents progress until resolved |
| **Limitation** | A known technical constraint (not a bug) |
| **Recommendation** | A proposed action, clearly marked as advice |
| **Unknown** | Needs investigation before any claim can be made |

This is the §4 evidence-discipline law of `zero-loss-guarantee-charter.md` made explicit as a reporting vocabulary.

---

## §3. Executive Summary table — mandatory status-report format

Status reports MUST lead with this table, readable by technical AND non-technical stakeholders, covering Frontend / Backend / Database / APIs / Infrastructure / Security / Performance / Monitoring:

| Area | Status | Risk | Coverage % | Notes |
|---|---|---|---|---|
| Backend | ✅ / ⚠️ / ❌ | Low / Med / High | NN% | one-line plain-English note |

`Status` uses ✅ (green) / ⚠️ (degraded) / ❌ (broken). Keep notes auto-driver-readable (no library jargon, no file paths in stakeholder-facing rows — per the 10 Telegram commandments in `operator-charter-forever.md` §D).

---

## §4. O(1) honesty (no false O(1) claims)

Evaluate O(1) for lookup, uniqueness, dedup, mapping, and latency EVERYWHERE. Where true O(1) is genuinely impossible, do NOT fake it — instead state: (a) the constraint, (b) the optimal practical alternative chosen, and (c) the trade-off. An inherently O(N) step is FLAGGED as O(N), never relabelled O(1). This mirrors `zero-loss-guarantee-charter.md` §1's O(1) envelope row.

---

## §5. Standing demands — pointers (DEFER to the charters for detail)

| Demand | Where the full contract lives |
|---|---|
| Extreme scenario / permutation / edge-case coverage | `zero-loss-guarantee-charter.md` §2 + `z-plus-defense-doctrine.md` (L1–L7) |
| Error / exception hardening (no silent failures; escalate alert→retry→halt) | `operator-charter-forever.md` rules 5/6 + `rust-code.md` |
| Retry / resilience — bounded exponential backoff, NO retry storms | `daily-universe-scope-expansion-2026-05-27.md` §4 + `disaster-recovery.md` |
| 22-test taxonomy + chaos / fault-injection | `testing.md` + `testing-scope.md` + `chaos_*.rs` suite |
| Automation-first (logs/queries/DB/health auto-accessible) | `operator-charter-forever.md` §A/§B + `wave-4-shared-preamble.md` §1 |
| Maximize safe parallelization (3-agent review, parallel research) | `operator-charter-forever.md` §E + `auto-effort-policy.md` |
| Honest "100%" wording + zero-loss envelope | `operator-charter-forever.md` §F + `zero-loss-guarantee-charter.md` §1 |

---

## §6. Auto-driver explanation

> Sir, the new rule has three simple parts. ONE: when you cook a dish, you don't walk away when it's served — you watch the customer eat it, confirm it's good, and only THEN start the next dish. TWO: when you report, you must say which things you SAW with your own eyes (Verified), which you only GUESSED (Assumed), and which might go WRONG (Risk) — never pretend a guess is a fact. THREE: when someone asks "is the shop fast?", if one counter can never be instant, you honestly say why and what the next-best speed is — you never lie and say "instant".

---

## Trigger: always loaded (this file lives under `.claude/rules/project/`). Reinforced on any session opening a PR, writing a status report, or making an O(1)/latency claim.
