---
name: quality
description: Run full quality gates — format, lint, test, and verify before commit
disable-model-invocation: true
allowed-tools: Bash
---

Run the complete quality gate pipeline. Stop at the first failure and report it.

## Steps

1. **Format:** `cargo fmt --all`
2. **Lint:** `cargo clippy --workspace --all-targets -- -D warnings`
3. **Test:** `cargo test --workspace`
4. **Docs:** `cargo doc --workspace --no-deps`

## Output

For each step, report PASS or FAIL. If any step fails, show the error output and stop.
Do NOT proceed to git commit — just report results.

If all 4 pass, output:
```
QUALITY GATES: ALL PASS
Ready to commit.
```
