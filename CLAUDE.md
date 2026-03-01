# CLAUDE.md — dhan-live-trader

> **Authority chain:** Tech Stack Bible V6 > this file > defaults. If neither covers a topic, ASK Parthiban.

## THREE PRINCIPLES

```
1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned
```

Every file, function, config decision must pass all three. No exceptions.

## PROJECT

- **Purpose:** O(1) latency live F&O trading system for Indian markets (NSE)
- **Language:** Rust 2024 Edition (stable 1.93.1)
- **Repo:** `https://github.com/SJParthi/dhan-live-trader` (single source of truth)
- **Runtime:** Docker everywhere. Mac (dev) → AWS c7i.2xlarge Mumbai (prod). Only `AWS_ENDPOINT_URL` differs.
- **Owner:** Parthiban (architect). Claude Code (builder).

## SESSION PROTOCOL

**Start:** git pull → read CLAUDE.md → read phase doc → git log -20 → Cargo.toml → cargo check → cargo test
**End:** Run `/quality` skill → commit → push → summary.

Do NOT read Bible at startup. Read it ONLY when adding/updating a dependency.

## WORKFLOW

Parthiban = architect. Claude Code = builder. Present plan → wait for approval → execute → show proof.
NEVER execute without approval. NEVER guess versions. Silence != approval.

## GIT

```
Branches: main | develop | feature/<phase>/<name> | fix/<desc> | hotfix/<desc>
Commit:   <type>(<scope>): <description>
Types:    feat, fix, refactor, test, docs, chore, perf, security
```

Every commit compiles + passes tests. One logical change per commit.

## CARGO

- Workspace deps in root Cargo.toml, crates use `{ workspace = true }`
- Exact versions ONLY from Bible. `^`, `~`, `*`, `>=` are BANNED. `cargo update` is BANNED.
- `edition = "2024"`, `rust-version = "1.93.1"` in every crate

## BANNED

Enforcement: `.claude/hooks/` (mechanical, blocks at commit). Rules: `.claude/rules/` (auto-loaded per path).
Quick ref: .env | bincode/Promtail/Jaeger-v1 | ^/~/\*/>=/:latest | brew | localhost | hardcoded values | .clone()/DashMap/dyn on hot | unbounded channels | println!/unwrap | cargo update

## TOKEN EFFICIENCY

- Never re-read files already in session. Parallelize reads. Keep responses short.
- No filler phrases. No repeating rules back. No essays.
- Bible: read ONLY when adding deps. PDFs: NEVER. Reference docs: ONLY when implementing that topic.

## COMPACTION

When compacting, always preserve: (1) list of all modified files (2) test/build results (3) current phase progress (4) unresolved errors or blockers (5) the three principles.

## CURRENT CONTEXT

**Phase:** Phase 1 — Live Trading System → `docs/phases/PHASE_1_LIVE_TRADING.md`
**Boot sequence:** CryptoProvider → Config → Logging → Auth → QuestDB → Universe → WebSocket → TickProcessor → API → TokenRenewal → Shutdown
