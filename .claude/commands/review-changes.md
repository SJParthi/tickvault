Review all uncommitted changes against the project's CLAUDE.md rules and quality standards.

## Steps

1. Run `git diff` to see all unstaged changes
2. Run `git diff --cached` to see staged changes
3. For each changed `.rs` file, check for:

### Banned Patterns (MUST FIX)
- `.unwrap()` or `.expect()` in production code
- `println!()` or `eprintln!()` (use `tracing` macros)
- `.clone()` on non-Copy types on hot path
- `DashMap` (use `papaya`)
- `Box<dyn Trait>` on hot path (use `enum_dispatch`)
- Unbounded channels or collections
- `format!()` on hot path
- Hardcoded values (use constants or config)
- `localhost` references (use config)

### Version Pinning (MUST FIX)
- Any `^`, `~`, `*`, `>=` in Cargo.toml dependencies
- Any `:latest` in Docker tags

### Security (MUST FIX)
- Hardcoded secrets, API keys, tokens
- `.env` file creation or references

### Quality (SHOULD FIX)
- Missing error context (use `.context()` from anyhow/thiserror)
- Missing `#[inline]` on tiny hot-path functions
- `unsafe` without `// SAFETY:` justification
- `#[allow(...)]` without `// APPROVED:` justification

4. Report findings in this format:
```
[MUST FIX] file:line — description
[SHOULD FIX] file:line — description
```

5. If no issues found: `REVIEW: ALL CHANGES CLEAN`
