# RAM Residency Stores — Error Codes (RAMSTORE-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `rest-candle-fold-error-codes.md` (FOLD-01 — the writer whose emit path
> feeds the spot rings) > `no-rest-except-live-feed-2026-06-27.md` (NO new
> market-data fetch — the stores mirror EXISTING flows) > this file.
> **Operator directive (2026-07-16, verbatim):** *"how can i believe you
> that you have all these already available in our in-memory app RAM —
> especially for the current day and even in the future last one month data
> should be entirely in memory app RAM, especially for trading decisions of
> entry and exit"* — refined by *"for only spots we will have minimum one
> month data because anyhow based on underlying spots alone only trading
> decision will be entered or exited — but option only for the current day"*
> and *"everything should be always available in our own questdb right —
> our entire one month should be stored and fetched from questdb even
> before premarket"*.
> **Companion code:** `crates/trading/src/in_mem/spot_bar_store.rs` (the
> month-deep spot bar rings), `crates/core/src/pipeline/chain_day_store.rs`
> (the current-day chain minute ring),
> `crates/app/src/market_ram_store_boot.rs` (install + chain rehydrate +
> stats/heartbeat task), the emit hooks in
> `crates/app/src/rest_candle_fold.rs` and the publish hook in
> `crates/app/src/option_chain_1m_boot.rs`,
> `crates/common/src/config.rs::MarketRamStoreConfig` (`[market_ram_store]`,
> serde default OFF; base.toml opts in),
> `crates/common/src/error_code.rs::ErrorCode::RamStore01Degraded`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `RamStore01*` variant verbatim —
> `RamStore01Degraded` and `RAMSTORE-01` appear below.

---

## §0. Why this code exists (the "believe it is in RAM" answer)

With both live feeds retired the runtime is REST-only; PR-1 (the FOLD-01
bar-fold writer) made the 21 `candles_*` tables populate from the official
`spot_1m_rest` bars. The operator's follow-up demand: the data used for
trading decisions must be RESIDENT IN PROCESS RAM — spots month-deep,
options current-day — not merely queryable in QuestDB. Two stores answer it,
both populated by EXISTING flows (zero new market-data fetch):

1. **Spot month-deep rings** (`SpotBarStore`): per (feed, sid, tf) rings of
   SEALED bars, capacity `spot_days` (default 35) × session bars/day
   (Σ 1,279 bars/day across the 21 TFs ≈ 17 MB at 8 slots × 35 days —
   test-asserted < 40 MB). WRITTEN at the fold's emit choke points (live
   seals upsert-by-ts; refold re-emits UPSERT in place; the boot catch-up's
   newest→oldest days block-PREPEND) — so pre-market spot rehydration IS
   PR-1's existing catch-up.
2. **Chain current-day ring** (`ChainDayStore`): per (feed, underlying)
   minute → the published `ChainMoneynessSnapshot` (strike/leg/ltp/
   moneyness — oi/volume/greeks stay in the `option_chain_1m` audit table,
   honestly NOT RAM-resident), CURRENT IST day only, cleared on day roll,
   bounded (≤405 minutes/slot + `chain_row_cap` rows/minute, default
   1_000). Boot-rehydrated from today's `option_chain_1m` rows via bounded
   hardened `/exec` windows so a mid-session restart has the morning back;
   rehydrated minutes NEVER overwrite live-published ones.

RAMSTORE-01 (`ErrorCode::RamStore01Degraded`) is the typed record of the
machinery degrading — never of market-data loss (QuestDB remains the
durable truth; a RAM degrade re-fills at the next boot).

## §1. RAMSTORE-01 — RAM residency store degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; live fills forward and the next boot re-fills — the operator
inspects, never manually rebuilds RAM).

**Trigger** (distinguished by the `stage` field):

| stage | Meaning |
|---|---|
| `install` | a store install was refused (duplicate install attempt — first-wins; defensive, loud) |
| `rehydrate_query` | a chain-rehydrate `/exec` window query failed (transport / non-2xx / oversize body) — that window is skipped; remaining windows still run |
| `rehydrate_parse` | a chain-rehydrate window's dataset failed to parse — skipped loudly |
| `rehydrate_truncated` | a chain-rehydrate window hit its explicit LIMIT — a partial window is NEVER trusted; skipped loudly (raise the bound in a reviewed PR, never silently) |
| `chain_truncated` | a live-published chain snapshot exceeded `chain_row_cap` rows and was truncated (kept prefix, counted — a hostile/runaway snapshot can never grow RAM unbounded) |
| `day_drop` | a STALE older-day chain publish arrived after the day rolled — dropped, never allowed to clear the live day |
| `task_respawn` | the supervised stats/rehydrate task died and was respawned (house `classify_join_exit` pattern; release builds abort per `panic = "abort"` — the honest TICK-FLUSH-01 envelope) |

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `RAMSTORE-01`; the `stage`
   names the failing leg (+ feed/underlying/window context fields).
2. `rehydrate_*` stages: QuestDB `/exec` was degraded at boot — run
   `make doctor` (cross-check BOOT-01/BOOT-02). The chain store starts
   shallow and live publishes fill forward; a restart after QuestDB
   recovers re-runs the full rehydrate.
3. `chain_truncated` sustained: the vendor chain outgrew the structural
   800-row publish bound — inspect the chain legs (CHAIN-02 ladder
   diagnostics) and raise `chain_row_cap` consciously if legitimate.
4. Depth check (the operator's "is the month actually in RAM?" question):
   read the gauges — `tv_ram_store_spot_days_depth{feed}` (the MINIMUM
   depth across the feed's slots — the guaranteed depth),
   `tv_ram_store_spot_bars_resident{feed}`,
   `tv_ram_store_chain_minutes_resident{feed}`,
   `tv_ram_store_estimated_bytes`. A flatlining
   `tv_ram_store_heartbeat_total` means the stats task is dead
   (`task_respawn` self-heals in unwind builds; release = restart).

**Honest envelope:** RAM depth is bounded by CAPTURED history —
`spot_1m_rest` holds only ~1-2 days as of 2026-07-16, so the rings reach
month-deep ORGANICALLY (~mid-Aug); the depth gauges show the honest fill
level and nothing is ever fabricated (audit Rule 11). Reads are
guarded-RwLock + binary-search (O(#slots ≤ 8) + O(log ring)) — NOT
lock-free and NOT claimed O(1); reads are COLD today because NO strategy
consumer exists (§28 boundary — the stores are the read contract only, the
`chain_snapshot` precedent, bound by the §38.8 decision-freshness gate when
a consumer ever ships with its own dated operator scope). The stores are
PROCESS-LOCAL: a restart rehydrates from QuestDB (PR-1 catch-up for spots +
the bounded chain rehydrate) — QuestDB is and remains the durable truth,
and the 15:40 IST tf-consistency verifier remains the DB-side exact-match
proof. The chain store deliberately holds ONLY the published decision rows
(strike/leg/ltp/moneyness + the snapshot header) — oi/volume/greeks are in
the `option_chain_1m` table, not in RAM.

**Delivery boundary (honest — no false-OK):** RAMSTORE-01 is
**log-sink-only** — NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list (the paging drift guard sees
no drift). The coded `error!`/`warn!` lines + the gauges/heartbeat are the
operator surface; adding a CloudWatch log-filter alarm is a flagged
follow-up (one map entry + a cost note — the SCOREBOARD-01 precedent).

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::RamStore01Degraded`
- `crates/trading/src/in_mem/spot_bar_store.rs` (ring core — pure, no emit
  sites; drops are counted via `tv_ram_store_dropped_total{reason}`)
- `crates/core/src/pipeline/chain_day_store.rs` (`chain_truncated` /
  `day_drop` emit sites)
- `crates/app/src/market_ram_store_boot.rs` (`install` / `rehydrate_*` /
  `task_respawn` emit sites + the gauges/heartbeat task)

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `RamStore01*` variant)
- `crates/trading/src/in_mem/spot_bar_store.rs`
- `crates/core/src/pipeline/chain_day_store.rs`
- `crates/app/src/market_ram_store_boot.rs`
- `crates/common/src/config.rs` (`MarketRamStoreConfig`) or
  `config/base.toml` `[market_ram_store]`
- Any file containing `RAMSTORE-01`, `RamStore01Degraded`, `SpotBarStore`,
  `ChainDayStore`, `market_ram_store`, or `tv_ram_store_`
