## Summary

<!-- What changed and why? 1-3 bullet points. -->

-

## Type of Change

<!-- Check all that apply -->

- [ ] `feat` — New feature or capability
- [ ] `fix` — Bug fix
- [ ] `refactor` — Code restructure, no behavior change
- [ ] `test` — Adding or updating tests
- [ ] `docs` — Documentation changes
- [ ] `chore` — Build, CI, dependency updates
- [ ] `perf` — Performance improvement
- [ ] `security` — Security fix or hardening

## Quality Gates

<!-- All must pass before merging -->

- [ ] `cargo fmt --check` — Zero formatting issues
- [ ] `cargo clippy -- -D warnings` — Zero warnings
- [ ] `cargo test --workspace` — 100% pass
- [ ] `cargo audit` — Zero known CVEs
- [ ] No secrets in code (SSM Parameter Store only)
- [ ] No hardcoded values (constants or config only)
- [ ] No `.unwrap()` in production code
- [ ] Doc comments on all public items

## Testing Done

<!-- How was this tested? What scenarios? -->

-

## CLAUDE.md Compliance

- [ ] Versions from Tech Stack Bible V6 only
- [ ] No banned crates or patterns
- [ ] Zero allocation on hot path (if applicable)
- [ ] O(1) or fail at compile time
