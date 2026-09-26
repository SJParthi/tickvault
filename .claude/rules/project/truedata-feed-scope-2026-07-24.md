# TrueData Fourth Feed — Live-Tick Feed Scope Extension (Operator Lock 2026-07-24) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/truedata-feed-scope-2026-07-24.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before touching any TrueData feed, parser (`crates/core/src/parser/truedata.rs`), config, SSM param, lock or plan.** It holds the verbatim operator quotes, the contract table (§2), the instance/reversibility contract (§3), and three dated evidence sections on the wire protocol: §9 (in-repo audit), §10 (official PDFs — supersedes §9), §11 (vendor SDK read end to end — "SUPERSEDES §9 and §10 wherever they conflict"). Protocol facts in §1/§2 are superseded by §9–§11; do not implement wire formats from this summary. Where this summary and the full file differ, the full file wins.

**Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `gdf-third-feed-scope-2026-07-13.md` > `groww-second-feed-scope-2026-06-19.md` > `websocket-connection-scope-lock.md` > `no-rest-except-live-feed-2026-06-27.md` > defaults. **Scope:** PERMANENT, TRIAL-FIRST.

**The rule (from §1):** a FOURTH feed, **TrueData**, feed `'truedata'`, **default OFF** (`feeds.truedata_enabled = false`, `#[serde(default)]`), implemented **natively in tickvault Rust** — NOTHING vendored from TrueData reference clients. Credentials READ-ONLY from SSM `Secret<String>` (`/tickvault/<env>/truedata/{user,password,endpoint}`), NEVER logged. Its tick stream drives the tick→timeframe aggregator (its own `FeedStrategy::Truedata` instance) through the unchanged resilience chain, writing the SAME shared tables tagged `feed='truedata'` with `feed` in every DEDUP key. A **1-session-per-key SSM named lock** (`/tickvault/<env>/instance-lock-truedata`) gates every connect, fail-closed. "**RAM-FIRST is absolute**": strategies/indicators/risk read ONLY from RAM. "TrueData drives NO strategy and places NO order." Implementation PRs B–F only after `.claude/plans/active-plan-truedata-feed.md` is operator-APPROVED.

**REJECT (§5, verbatim headlines):**
- Starts ANY implementation PR (B–F) while the plan `**Status:**` is still DRAFT.
- Ships `feeds.truedata_enabled = true` as the DEFAULT without a fresh dated quote.
- Creates any `truedata_*` parallel DATA table, writes a shared-table row without `feed='truedata'`, or omits `feed` from a DEDUP key.
- Vendors/imports ANY TrueData reference-client code (any language) or adds a sidecar for TrueData (native Rust only).
- Subscribes the TrueData 1min/5min BAR streams instead of deriving timeframes from ticks, or persists a bar the aggregator did not derive.
- Reads market data / a decision input from QuestDB in any indicator/strategy/risk path.
- Opens a TrueData connection without holding `instance-lock-truedata`, or fights the "User Already Connected" refusal with a reconnect storm.
- Puts parsing/logic/blocking on the socket READ task (must drain → WAL → ring only).
- Drops the per-symbol Tick Sequence gap check.
- Wires TrueData into any strategy/order/risk path; gives it a weaker durable floor than Groww/GDF.
- Logs the user/password, embeds them anywhere but the connect URL built from SSM, or stores them outside SSM + in-memory `Secret<String>`.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote."
