# Contributing to tickvault

## Setup

```bash
git clone https://github.com/SJParthi/tickvault.git
cd tickvault
./scripts/bootstrap.sh
```

Bootstrap handles everything: Rust toolchain, quality tools, git hooks, Docker services, AWS SSM credentials, QuestDB tables, and verification.

## Three Principles

Every change must pass all three. No exceptions.

1. **Zero allocation on hot path** — no Box, Vec, String, format!, clone on tick processing paths
2. **O(1) or fail at compile time** — no linear search, no O(n) operations on hot path
3. **Every version pinned** — exact versions only, no `^`, `~`, `*`, `>=`

## Branch Naming

```
main                        # single branch (until AWS deployment)
feature/<phase>/<name>      # new work
fix/<description>           # bug fixes
claude/<id>                 # Claude Code sessions
```

## Commit Messages

Conventional commit format:

```
<type>(<scope>): <description>

Types: feat, fix, refactor, test, docs, chore, perf, security
Scope: the crate or area (core, trading, app, hooks, ci)
```

Examples:
- `feat(core): add WebSocket reconnection with exponential backoff`
- `fix(trading): correct position reconciliation on partial fills`
- `test(core): add Loom concurrency tests for tick dedup`

## Quality Gates

Before submitting a PR, run:

```bash
# Quick check
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace

# Or use the Claude Code skill
/quality
```

CI enforces checks on PRs to main. All must pass.

## Code Review Checklist

- [ ] Versions from Tech Stack Bible V6 only
- [ ] No banned patterns (unwrap, println, clone on hot path, etc.)
- [ ] Zero allocation on hot path (if applicable)
- [ ] O(1) or fail at compile time
- [ ] No secrets in code (AWS SSM Parameter Store only)
- [ ] Doc comments on all public items

## Key References

- [CLAUDE.md](CLAUDE.md) — project rules and conventions
- [docs/architecture/tech-stack-bible.md](docs/architecture/tech-stack-bible.md) — approved dependencies and versions
- [docs/phases/phase-1-live-trading.md](docs/phases/phase-1-live-trading.md) — current phase

## Owner

All code changes require review from [@SJParthi](https://github.com/SJParthi) (CODEOWNERS).
