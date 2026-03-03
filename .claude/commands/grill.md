Adversarial code review. Find every possible issue. Don't ship until this passes.

## Mindset

You are a hostile reviewer. Your job is to find problems, not to be polite. Assume every line of changed code has a bug until proven otherwise.

## Steps

1. Run `git diff HEAD` to see all uncommitted changes (or `git diff main...HEAD` for all branch changes)
2. For EACH changed file, ruthlessly check:

### Correctness
- Edge cases: what happens with zero, one, max, negative, empty, None?
- Off-by-one errors in loops, slices, ranges
- Integer overflow/underflow possibilities
- Race conditions in concurrent code
- Error handling: are all error paths covered? Any silent swallowing?

### Performance (Three Principles)
- Any allocation on hot path? (Box, Vec, String, format!, clone, collect)
- Any O(n) where O(1) is possible? (linear search, contains on Vec)
- Any blocking I/O on async path?
- Any unbounded growth? (channels, collections, logs)

### Security
- Any untrusted input used without validation?
- Any secret/credential in code?
- Any SQL/command injection vector?
- Any TOCTOU race?

### Rust-Specific
- Any `unwrap()` that could panic in production?
- Any `unsafe` without SAFETY justification?
- Missing error context on `?` operator?
- Lifetime issues or unnecessary clones?

### Architecture
- Does this change follow existing patterns in the codebase?
- Is there existing code that should be reused instead of duplicating?
- Are new public APIs minimal and well-designed?

3. Report findings by severity:
```
[CRITICAL] file:line — description (must fix before shipping)
[HIGH]     file:line — description (should fix)
[MEDIUM]   file:line — description (consider fixing)
[LOW]      file:line — description (nitpick)
```

4. End with a verdict:
- `VERDICT: SHIP IT` — no critical or high issues found
- `VERDICT: FIX FIRST` — critical/high issues must be resolved
