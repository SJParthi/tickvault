# Operator Charter — FOREVER (auto-loaded every session) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/operator-charter-forever.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file at session start and before any PR, plan item, Telegram message, or design.** It holds the verbatim operator charter, the 15-row + 7-row guarantee matrices (§C), the Telegram 10 commandments (§D), the 3-agent review mandate (§E), the full PR loop (§H), the WebSocket scope history (§I), the mechanical enforcement chain and the read-this-first protocol. Where this summary and the full file differ, the full file wins. Amend the full file first, then keep this summary in sync.

**Authority:** CLAUDE.md > this file > defaults. **Scope:** every Claude Code session, Cowork task, branch, PR, plan item, task file, commit — FOREVER. PERMANENT; cannot be superseded.

**Binding contract (headlines):**
- **A.** SessionStart hooks run doctor / snapshot / health / context / sanity. "If ANY step fails: fix the underlying issue. Do NOT bypass with skip-flags."
- **C.** Every plan item / task file MUST carry the 15-row + 7-row guarantee matrix (`.claude/hooks/per-item-guarantee-check.sh`).
- **D.** Every Telegram message follows the 10 commandments: plain English, no library names, no file paths, no version numbers, status emoji, specific numbers, action verbs when degraded, one message = one decision, IST 12-hour timestamps, severity emoji first. Litmus: a 60-year-old auto-driver knows "OK or not OK + what to do" in 5 seconds.
- **E.** Changes touching > 3 crates or > 1,000 LoC: run hot-path, security and hostile general reviewers IN PARALLEL; fix every CRITICAL/HIGH before opening the PR.
- **F.** Any "100% guarantee" MUST be qualified: "100% inside the tested envelope, with ratcheted regression coverage: …" (exact wording in the full file). "Anything stronger ("WebSocket never disconnects" / "QuestDB never fails" without envelope) = REJECT IN REVIEW."
- **G.** Every artefact includes a kid-friendly + table explanation.
- **H.** PR Completion Protocol: one PR at a time, finished and merged to `main` before the next; draft PR → tick plan box in the same diff → ready → auto-merge → subscribe → fix red CI → verify merged → then next. FORBIDS: opening PR #2 before PR #1 merges, ticking `[x]` before merge, manual merge skipping CI, `--no-verify` / `--no-gpg-sign`.
- **I.** WebSocket scope is governed by `websocket-connection-scope-lock.md` (read it; its dated amendments supersede the original 2026-05-15 two-connection table).

**The ALWAYS-ON rules (cheat sheet):** common runtime (Mac dev = AWS prod); RAM-first hot path, no DB queries from indicator/strategy/risk paths; composite-key uniqueness `(security_id, exchange_segment)` everywhere; DEDUP UPSERT KEYS on every QuestDB table + `ALTER ADD COLUMN IF NOT EXISTS`; every `error!` carries `code = ErrorCode::X.code_str()`; flush/persist/drain failures use `error!`, never `warn!`; counter panels wrap in `increase()`/`rate()`; every new pub fn has a call site; market-hours-aware tokio tasks; edge-triggered alerts only; NO false-OK signals; one PR open at a time.

**"Anything that needs to be true must be machine-checked. Hand-wave = REJECT."** The `block-bypass.sh` hook blocks `--no-verify`, `git stash push/pop/apply/save`, `--dangerously-skip-permissions`, `gh pr merge --admin`, and force-push to main/master.
