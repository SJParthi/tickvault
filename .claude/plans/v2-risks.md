# Movers 22-TF v3 — Risks (8 movers risks + 2 expiry-rollover risks)

## 8 movers-side risks (unchanged from v2)

| # | Risk | v3 mitigation |
|---|---|---|
| 1 | QuestDB ILP TCP disconnect mid-snapshot | Movers-specific rescue ring extending existing `RescueRing` |
| 2 | mpsc(8192) saturation under burst | Drop-NEWEST + 0.1% SLA Telegram alert |
| 3 | Scheduler clock drift > 100 ms | `tv_movers_snapshot_lag_seconds{timeframe}` gauge + alert |
| 4 | 22 tasks all panic | `MoversSupervisor` respawns within 5s + `MOVERS-22TF-02` |
| 5 | IDX_I prev_close missing for new index mid-day | 3-tier fallback (cache → table → skip, never NaN) |
| 6 | F&O contract expires intraday | `is_active_today` filter using `FnoUniverse.expiry` |
| 7 | Movers tables not in partition manager | All 22 added to `HOUR_PARTITIONED_TABLES` |
| 8 | SEBI retention classification | Documented as 90d operational, NOT 5y |

## 2 NEW expiry-rollover-side risks

| # | Risk | v3 mitigation |
|---|---|---|
| 9 | Constant flip from 1 → 0 silently breaks existing 5 ratchet tests | **Rewrite all 5 tests** in same commit; new tests assert `T-1 KEEPS nearest` (was: rolls); CI catches drift |
| 10 | Live subscription churn on expiry day morning | Rollover triggered at boot, NOT mid-session (existing planner only runs at boot + Phase 2 dispatch); `subscription_audit_log` records the swap; depth-20 + depth-200 unaffected (NIFTY/BANKNIFTY indices, not stocks) |

## Chaos test (unchanged from v2)

| Item | Detail |
|---|---|
| File | `crates/storage/tests/chaos_movers_22tf_throughput.rs` |
| Asserts | ≥1.0M rows/sec sustained over 60s; ≤0.1% drop rate; no ILP RST |
| Trigger | `CHAOS=1 cargo test --release` (CI nightly) |
| Fallback if fails | Reduce to 11-writer pool OR raise QuestDB ILP connection limit 20 → 32 |

## Failure-mode coverage — 12 modes, 12 mitigations

| # | Failure | Mitigation |
|---|---|---|
| 1 | Single snapshot task panic | `MoversSupervisor` respawn |
| 2 | All 22 tasks panic | Supervisor respawns each |
| 3 | One writer ILP RST | Per-writer reconnect |
| 4 | All writers ILP RST | Rescue ring buffers all 22 streams |
| 5 | mpsc full | Drop-NEWEST + 0.1% SLA alert |
| 6 | Scheduler clock drift | Drift gauge + alert |
| 7 | App restart mid-day | papaya rebuilds; IDX_I from `previous_close` table |
| 8 | New IDX_I mid-day | 3-tier fallback |
| 9 | F&O expires intraday | `is_active_today` filter |
| 10 | EBS fills | Partition manager rotation |
| 11 | **Depth cache empty for instrument** (e.g., far-OTM option not subscribed to depth) | Depth columns set to NULL; query filters via `WHERE best_bid IS NOT NULL` for spread tabs |
| 12 | **Stock F&O on expiry day** | **Rollover constant flip 1 → 0; subscription planner picks next expiry at boot** |

## Memory + capacity (re-verified for 26-column schema)

| Item | Calculation | Result |
|---|---|---|
| `MoverRow` size | 26 cols, ArrayString<16>×3 + numerics + DATE + Copy padding | ~296 B |
| Arena per timeframe | 30K capacity × 296 B | 8.9 MB |
| 22 arenas total | | 196 MB |
| 22 mpsc(8192) buffers | 22 × 8192 × 296 B | 52 MB |
| Papaya tracker | ~24K × ~80 B | 1.9 MB |
| MarketDepthCache (read-only) | already exists, no growth | 0 |
| **Total movers memory** | | **~250 MB** within 1 GB app budget |

| Item | Calculation | Result |
|---|---|---|
| Daily row volume | sum 22 cadences × 24,324 instruments | ~125M rows/day |
| 90-day storage @ ~180 B/row compressed | | ~22 GB |
| Fits 100 GB EBS | yes (other tables ~30 GB) | ✓ |

## 5 open questions for Parthiban

| # | Question | v3 default |
|---|---|---|
| 1 | Approve 9 extra candle views (4m, 6m, 7m, 8m, 9m, 11m, 12m, 13m, 14m)? | YES |
| 2 | Approve full-universe per-snapshot persistence (~125M rows/day, ~22GB/90d)? | YES |
| 3 | Approve unified `movers_{T}` table per timeframe? Deprecate `top_movers`/`option_movers`/`preopen_movers` after dashboard migrates | YES |
| 4 | Approve 22 separate writers? Bumps QuestDB ILP connections from ~6 used to ~28; need limit 20 → 32 | YES |
| 5 | Approve chaos test as merge-blocker for 1M rows/sec claim? | YES |
| 6 | **NEW** — Approve expiry rollover policy change (T-1 → T-only, only for stock F&O, indices unchanged)? | **YES (per Parthiban call this session)** |
| 7 | **NEW** — Approve depth integration via Option A (columns on movers) + Option C (6 dashboard panels)? Option B (full ladder snapshots) rejected as too heavy | **YES (per Parthiban call this session)** |
