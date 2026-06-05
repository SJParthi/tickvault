# Design-First Wall — no implementation without an approved extensive plan

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file > defaults.
> **Scope:** PERMANENT. Every PR, every sub-PR, every task, every session.
> **Operator demand (2026-06-05, verbatim intent):** *"until or unless
> thoroughly designing/deriving an extensive plan covering everything we
> should NEVER EVER go ahead with implementation."*
> **Status:** mechanically enforced (not a wish) by `.claude/hooks/plan-gate.sh`.
> **Auto-load:** always loaded (path is under `.claude/rules/project/`).

---

## The rule (one line)

**If a change touches implementation source (`crates/<x>/src/**.rs`), an
APPROVED active plan covering Design · Edge Cases · Failure Modes · Test Plan
· Rollback · Observability MUST exist FIRST — otherwise the push is BLOCKED.**

This sits on top of `plan-enforcement.md` (which governs plan format/lifecycle)
and turns the "design first" half into a hard, fail-closed gate.

---

## What the gate checks (`plan-gate.sh`)

| Step | Logic | Outcome |
|---|---|---|
| 1 | Compute changed files: `origin/main...HEAD` + working tree + staged + **untracked** (NUL-safe) | — |
| 2 | Any match `^crates/[A-Za-z0-9_-]+/src/.*\.rs$`? | No → **PASS** (docs/CI/hooks/config/deps exempt) |
| 3 | Latest commit body has `PLAN-EXEMPT: <reason>` **and** ≤1 impl file changed? | Yes → **PASS** (logged, auditable); multi-file → **BLOCK** |
| 4 | Any `.claude/plans/active-plan*.md` exists? | No → **BLOCK** |
| 5 | Some plan has all 6 required `##` sections, **each with a non-empty body**? | None → **BLOCK** (names which) |
| 6 | That plan's `**Status:**` is `APPROVED` / `IN_PROGRESS` / `VERIFIED`? | No → **BLOCK** |
| 7 | That plan **references a changed crate** (name or file path)? | No → **BLOCK** |

The 6 required sections: **Design**, **Edge Cases**, **Failure Modes**,
**Test Plan**, **Rollback**, **Observability** — each must have content, not
just a heading. All `active-plan*.md` files are scanned; at least one must
fully satisfy steps 5–7 for the specific change.

Exit codes: `0` = allowed, `2` = BLOCK (errors on stderr).

### Adversarial-review hardening (2026-06-05)

A security-reviewer + hostile bypass-hunt pass on the gate itself found and
fixed these false-negatives **before** it shipped:

| ID | Hole it closed |
|---|---|
| C1 | A brand-new untracked `crates/x/src/evil.rs` (never `git add`ed) was invisible to all diff sources → would PASS. Now counted via `git ls-files --others`. |
| H2 | Crate-name regex `[a-z_]+` missed `core2` / `MyCrate` / `foo-bar`; broadened to `[A-Za-z0-9_-]+`. |
| H3 | `head -1` validated a change against one arbitrary (possibly stale) plan; now scans ALL plans and the satisfying one must reference the changed crate. |
| M4 | Six empty headings used to satisfy the section check; now each section needs a non-empty body. |
| M5 | `PLAN-EXEMPT` could cover a whole multi-file feature; now capped to ≤1 implementation file. |
| L6 | Base ref resolves `origin/main → main`; NUL-safe path handling throughout. |

---

## Where it is wired

| Entry point | Gate slot | Always runs? |
|---|---|---|
| `.claude/hooks/pre-push-gate.sh` (fast PreToolUse on `git push`) | `[0]` | yes |
| `scripts/git-hooks/pre-push` (version-controlled git hook) | `[0/11]` | yes — never dedup-skipped |

Both call `.claude/hooks/plan-gate.sh "$PROJECT_DIR"` and fail the push on exit 2.

---

## Honest envelope (no illusion)

This gate guarantees the **known** design surface is covered before code is
written. It does **not**:

- invent unknown-unknowns the plan author didn't think of;
- judge plan *quality* beyond section presence + APPROVED status;
- fire anywhere it is not wired (it lives in the two push hooks above).

It is fail-CLOSED on a real implementation change with no/incomplete plan, and
PASS on non-implementation changes. The `PLAN-EXEMPT:` escape hatch is loud and
logged precisely so reviewers can confirm it is not hiding un-designed work.

---

## The escape hatch (auditable, loud)

For a genuinely trivial logic change, put this in the latest commit body:

```
PLAN-EXEMPT: <one-line reason — e.g. fix typo in log string, no logic change>
```

Every use is echoed by the gate with a NOTE to review it. Abuse is visible in
the commit history; that visibility is the deterrent.

---

## Proof it actually blocks (not just claims to)

`.claude/hooks/plan-gate.selftest.sh` builds throwaway git repos and asserts all
six behaviours — run it any time:

```
$ bash .claude/hooks/plan-gate.selftest.sh
  ok   : impl change + no plan -> BLOCK (exit 2)
  ok   : impl change + PLAN-EXEMPT (1 file) -> PASS (exit 0)
  ok   : PLAN-EXEMPT covering 2 impl files -> BLOCK (exit 2)
  ok   : impl change + complete APPROVED plan -> PASS (exit 0)
  ok   : impl change + incomplete plan -> BLOCK (exit 2)
  ok   : complete plan but Status=DRAFT -> BLOCK (exit 2)
  ok   : docs-only change -> PASS (exit 0)
  ok   : untracked NEW .rs file, no plan -> BLOCK (C1) (exit 2)
  ok   : hollow plan (empty sections) -> BLOCK (M4) (exit 2)
  ok   : digit-crate (core2) src, no plan -> BLOCK (H2) (exit 2)
  ok   : plan references wrong crate -> BLOCK (H3) (exit 2)
  plan-gate self-test: 11 passed, 0 failed
```

---

## Auto-driver explanation

> Sir, imagine the juice shop. The new rule: the boy is NOT allowed to switch on
> the grinder until the full recipe card is on the counter — what fruit, what
> can go wrong, how to taste-check, how to undo a bad batch, how to watch it.
> No recipe card = grinder stays OFF. Only for wiping a spill (a tiny thing) can
> he write a one-line note "just wiping, no cooking" — and that note is kept so
> the manager can check he's not sneaking in un-planned cooking.

---

## Trigger (auto-loaded paths)

Always loaded. Reinforced on any session editing `.claude/hooks/plan-gate.sh`,
`.claude/hooks/pre-push-gate.sh`, `scripts/git-hooks/pre-push`,
`.claude/plans/active-plan*.md`, or any `crates/*/src/*.rs`.
