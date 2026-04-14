# Testing Scope Rule — Block-Scoped by Default

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Governs which tests from the 22-test standard (`testing.md`) run
> for a given session / change. Applies to ALL implementation sessions.
> **Trigger:** This rule is always loaded.

## Purpose

Stop wasting tokens and wall-clock time running the full workspace 22-test
battery for every small change. When you're editing three functions in
`crates/storage/`, you do not need to re-run the `crates/trading/` property
tests, the `loom` concurrency models, the `cargo-mutants` run, and the
OpenTelemetry exporter integration tests. That full sweep is for CI, not
interactive development.

## The Rule

**Default test scope = only the crates touched in the current diff.**

Workspace-wide (full 22-test) execution happens ONLY when one of:

1. The user types `/full-qa` (or equivalent slash command)
2. The user sets `FULL_QA=1` in the environment
3. CI is running the post-merge pipeline
4. A banned-pattern / hot-path scan surfaces a violation that needs full
   workspace re-verification (enforcement, not convenience)

Otherwise, scoped execution is mandatory.

## Scoped Execution Algorithm

```
1. Compute changed files = git diff --name-only HEAD + unstaged tracked files
2. Extract touched crates from paths matching crates/<name>/...
3. For each touched crate, run:
     cargo test -p tickvault-<name> --lib --tests
4. If the change touched .claude/hooks/**, also run: bash .claude/hooks/plan-verify.sh
5. If the change touched crates/common/**, escalate to workspace:
     cargo test --workspace  (because everyone depends on common)
6. If no crates touched (docs/config only), skip cargo tests entirely
```

This is implemented in `.claude/hooks/scoped-test-runner.sh`.

## When Scoped Testing is Safe

Scoped testing is safe when:

- All changes stay inside one or a few crates
- No public API in `crates/common/` changed
- No shared trait signatures changed
- No `Cargo.toml` workspace dependency versions changed
- No `.claude/rules/` file changed that would affect enforcement scans

For every other case, run `/full-qa`.

## When Scoped Testing is NOT Enough

Escalate to `/full-qa` when:

- A `crates/common/` type was modified (downstream crates may break)
- A binary protocol byte offset or packet size constant was changed
- A `DEDUP_KEY_*` constant was modified (invalidates stored data assumptions)
- The change touches more than 3 crates
- Any banned-pattern scan triggered during the session

## Benefits (for this retail solo trading product)

1. **Token efficiency.** One scoped run = ~30s on a warm cache. Workspace =
   10-15min. On a paid session, that compounds fast.
2. **Developer cadence.** Incremental test feedback stays fast; you don't
   wait for irrelevant tests.
3. **Signal clarity.** When a scoped run fails, it is GUARANTEED to be
   related to your change — no spurious failures from unrelated subsystems.
4. **Still safe.** Every merge still hits the full 22-test battery in CI,
   plus coverage gates. Nothing is weakened — only the local feedback loop
   gets faster.
5. **Prevents "every test ran successfully" hallucination.** The runner
   reports exactly which crates were tested and which were skipped, so
   there is no ambiguity about what was verified.

## Enforcement

- Hook `.claude/hooks/scoped-test-runner.sh` is the single entry point.
- `make scoped-check` wraps the hook for convenience.
- The hook prints a scope summary at the top of its output:
  ```
  SCOPE: storage, common (escalated)
  SKIP : api, app, core, trading
  REASON: change in crates/common/ forces workspace test
  ```
- `pre-push-gate.sh` runs the scoped runner by default; `FULL_QA=1 git push`
  forces workspace.
- This rule does NOT relax any of `testing.md` requirements. It only
  changes *when* and *where* those requirements apply for a given diff.

## Relation to 22-Test Standard

The 22-test standard (`.claude/rules/project/testing.md`) defines WHAT
must exist: unit, integration, property, loom, dhat, fuzz, mutation,
sanitizers, coverage, adversarial, etc. For the touched crate, ALL 22
categories still apply. The scope rule only says: don't re-run them for
untouched crates in the same session.

## Mechanical Checks Added by This Rule

1. `.claude/hooks/scoped-test-runner.sh` — reads git diff, computes scope,
   runs only relevant `cargo test -p`.
2. `Makefile` target `scoped-check` — wraps the hook.
3. `testing.md` references this file at the top so reviewers know the
   default scope.

## What This Prevents

- Repeated full-workspace test runs on trivial edits
- Token waste from re-executing unrelated integration tests
- False confidence from "workspace tests passed" when the change couldn't
  have affected most of the workspace anyway
- Frustration from 15-minute test cycles per change

## What This Does NOT Do

- Does not skip any test in CI
- Does not weaken coverage thresholds (still 100% per
  `quality/crate-coverage-thresholds.toml`)
- Does not allow merging without running the full battery somewhere
- Does not apply to the changed crate's OWN tests (those always run)

## Override

To run the full 22-test battery manually at any time:

```bash
FULL_QA=1 make scoped-check
# or
cargo test --workspace
# or
/full-qa
```
