---
name: quality
description: Run full quality gates — all 8 checks matching CI plus local-only gates
disable-model-invocation: true
allowed-tools: Bash
---

Run the complete quality gate pipeline. Stop at the first failure and report it.

## Steps

1. **Format:** `cargo fmt --all`
2. **Lint:** `cargo clippy --workspace --all-targets -- -D warnings -W clippy::perf`
3. **Test:** `cargo test --workspace`
4. **Docs:** `cargo doc --workspace --no-deps`
5. **Banned Patterns:** `.claude/hooks/banned-pattern-scanner.sh . "$(find crates -name '*.rs' -not -path '*/target/*')"`
6. **Test Count Guard:** `.claude/hooks/test-count-guard.sh .`
7. **Security:** `cargo audit` (if installed, else SKIP)
8. **Deny:** `cargo deny check` (if installed, else SKIP)

## Output

For each step, report PASS, FAIL, or SKIP. If any step fails, show the error output and stop.
Do NOT proceed to git commit — just report results.

If all steps pass, output:
```
QUALITY GATES: ALL 8 PASSED
Ready to commit.
```
