# Stream-Resilient Comprehensive Automation Protocol

> **Authority:** CLAUDE.md > this file.
> **Scope:** Every Claude Code interaction in this repo. Permanent project rule.
> **Created:** 2026-04-27 (gap response to repeated `API Error: Stream idle timeout - partial response received`).
> **Trigger:** Always loaded.

## Why this rule exists

When a Claude Code response streams a single large output (e.g., a full 6,800-LoC plan inline, or a 90-cell table block, or a multi-thousand-token narrative), the upstream HTTP stream can stall on a network blip and the client gives up. The user sees "API Error: Stream idle timeout — partial response received" and has to retry. In a long planning session this is expensive in tokens, time, and trust.

This rule defines the protocol that prevents stream timeouts at the source: **chunk small, persist files, parallel agents, file-first chat**.

## The 12 mechanical rules

| # | Rule | Mechanism | Proof / ratchet |
|---|------|-----------|-----------------|
| B1 | **File is the source of truth, chat is a pointer** | Every plan / decision / multi-table content goes to `.claude/plans/*.md`; the chat carries only a 1-page summary + file paths | Pre-push gate verifies any path cited in the chat exists |
| B2 | **Chunk every output to ≤ 2K tokens** | Tables, bullet lists, file references — no prose paragraphs | Code-review hook scans turns; warns if any single tool/text block > 2K tokens |
| B3 | **Spawn ≥ 3 specialist agents in parallel** for any decision touching > 3 crates | Each agent `report under 400 words`, writes to `.claude/plans/research/<topic>.md` | `make plan-research` aggregates the files |
| B4 | **Background agents for ≥ 5 min work** | `run_in_background: true`; main thread continues with planning while agents run | Notification on completion; results in file |
| B5 | **Every Telegram event has a coalescing window** | Bucket: max 1 event/topic/60s; bursts → 1 summary | `tv_telegram_dropped_total` Prom counter |
| B6 | **Every error has a typed `ErrorCode` + runbook + triage YAML rule** | Tag-guard meta-test fails build if `error!` mentions code without `code = ErrorCode::X.code_str()` | `crates/common/tests/error_code_*.rs` |
| B7 | **Every persistence path emits Prometheus + tracing + Telegram on failure** | 5-sink fan-out (stdout / app.log / errors.log / errors.jsonl / Telegram+SNS) | `crates/storage/tests/error_level_meta_guard.rs` |
| B8 | **Every plan item passes the 9-box checklist** | typed event, ErrorCode, tracing+code field, Prom counter, Grafana panel, alert rule, call site, triage rule, ratchet test | Plan-verify hook scans plan file for 9 ticks per item |
| B9 | **Every hot path has DHAT (zero-alloc) + Criterion bench + budget** | `quality/benchmark-budgets.toml` 5% regression gate; CI runs both | `scripts/bench-gate.sh` |
| B10 | **Every QuestDB table has DEDUP UPSERT KEYS + ALTER ADD COLUMN IF NOT EXISTS** | Self-heal at boot | `dedup_segment_meta_guard.rs` |
| B11 | **Every public fn has a test or `// TEST-EXEMPT:` line** | Pre-push gate 6 | `pub-fn-test-guard.sh` |
| B12 | **Every feature has a `config/base.toml` toggle + rollback test** | Flip-to-off path is a tested code path, not aspirational | `feature_flag_rollback_guard.rs` |

## File-first chat — concrete protocol

For any response that would exceed 2K tokens of streamed text:

1. **Write the bulk to a file.** Use the `Write` tool to put the full content in `.claude/plans/`, `docs/`, or `data/notes/`.
2. **In the chat reply, include only:**
   - 1-sentence summary of what was written
   - The path to the file
   - Up to 1 small table (≤ 30 rows) for at-a-glance context
   - Any decision needed from the operator (in a tight bullet list)
3. **Never paste the same content into both file and chat.** The file is canonical; the chat is the pointer.

## Parallel agents — concrete protocol

For any decision that would otherwise force the main session to do > 3 file searches or > 3 grep commands:

1. **Identify the dimensions** (e.g., security, performance, dependencies, testing, resilience, automation).
2. **Spawn one specialist agent per dimension** with `run_in_background: true` and a strict `report under 300-400 words` instruction.
3. **Move on with planning** while agents run. Report progress to operator on each completion notification.
4. **Synthesize agents' findings into one consolidated table** — never paste raw agent output.

## Per-PR-set protocol

For any change touching > 3 crates or > 1,000 LoC:

1. **Split into ≤ 3 sub-PRs** with explicit branch sequencing.
2. **Each sub-PR ≤ 30 files / ≤ 3,000 LoC** (reviewer must complete in one sitting).
3. **Plan files live in their own commit** before any production code changes.
4. **Wave 1 → Wave 2 (rebased on Wave 1's main merge) → Wave 3 (rebased on Wave 2's main merge).**

## What this rule prevents

- "API Error: Stream idle timeout — partial response received" — chunked outputs ≤ 2K never trigger
- Lost work when a long stream fails mid-response — file is already written before chat sends
- Single-PR review fatigue — 3-Wave split keeps each PR focused
- Hidden context loss between sessions — files persist across `/compact` and session boundaries

## Enforcement

- This rule is auto-loaded for every Claude Code session in this repo.
- The pre-push gate `bash .claude/hooks/plan-verify.sh` checks B1 (file-first) for any plan referenced in the chat.
- `scripts/bench-gate.sh` enforces B9.
- `crates/common/tests/error_code_*.rs` enforces B6.
- `crates/storage/tests/dedup_segment_meta_guard.rs` enforces B10.
- `crates/storage/tests/error_level_meta_guard.rs` enforces B7.

## Trigger

This rule activates on every session start. Always loaded.
