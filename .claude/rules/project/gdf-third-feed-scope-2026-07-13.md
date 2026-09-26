# GDF Third Feed — Pluggable-Feeds Scope Extension (Operator Lock 2026-07-13) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/gdf-third-feed-scope-2026-07-13.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before touching any GDF feed, config, table, lock or plan code.** It holds the verbatim operator quote, the locked contract table (§2), the market-data pull inventory (§3), the honest envelope (§6) and all evidence citations. Where this summary and the full file differ, the full file wins. Amend the full file first (rule-file-first law), then keep this summary in sync.

**Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `groww-second-feed-scope-2026-06-19.md` > `websocket-connection-scope-lock.md` > `no-rest-except-live-feed-2026-06-27.md` > defaults. **Scope:** PERMANENT.

**The rule (from §1):** a THIRD market-data feed, **GDF** (Global Data Feeds / GFDL), feed `'gdf'`, **default OFF** (`feeds.gdf_enabled = false`, `#[serde(default)]`), TRIAL-FIRST. Implemented **natively in tickvault Rust** — NOTHING vendored from GFDL SDKs. Reuses the resilience chain AS-IS (capture-at-receipt / WAL-before-broadcast → ring → spill → DLQ → its own aggregator instance) and writes the **SAME shared tables** with every row tagged `feed='gdf'` and `feed` in every DEDUP key. A **1-session-per-key SSM named lock** (`/tickvault/<env>/instance-lock-gdf`) gates every connect, fail-closed. "GDF drives NO strategy and places NO order — capture + candles + cross-verification ONLY during the trial and until a fresh dated quote says otherwise." Implementation PRs B–G may start only once `.claude/plans/active-plan-gdf-feed.md` is operator-APPROVED (design-first wall).

**REJECT (§5, verbatim headlines):**
- Starts ANY implementation PR (B–G) while the plan `**Status:**` is still DRAFT.
- Ships `feeds.gdf_enabled = true` (or `gdf.mode` auto-enabling) as the DEFAULT without a fresh dated quote.
- Creates any `gdf_*` parallel DATA table, or writes a shared-table row without `feed='gdf'`, or omits `feed` from a DEDUP key.
- Vendors/imports ANY GFDL SDK code or adds a Python sidecar for GDF.
- Bulk-ingests the FULL firehose into QuestDB.
- Adds any GDF REST caller, or any WS market-data function beyond §3 without a dated §3 edit FIRST.
- Routes GetHistory output into `ticks` / `candles_*` / `historical_candles`.
- Opens a GDF connection without holding the `instance-lock-gdf` named lock, or fights the "key already in use" reject with a reconnect storm.
- Wires GDF into any strategy/order/risk path; gives GDF a weaker durable floor than Groww/Dhan.
- Logs the API key, embeds it in a URL, or stores it outside SSM + in-memory `Secret<String>`.
- Claims TBT, sub-second timestamps, or depth for GDF anywhere.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote."
