# Implementation Plan: Boot Telegram Hygiene + PREVOI-01 Avoidance + Movers Log Retire

**Status:** VERIFIED
**Date:** 2026-05-09
**Approved by:** Parthiban — "FIx and implement everyhtign", "why zip jsut avoid that", "Also retire LogCategory::Movers"
**Branch:** `claude/migrate-bhavcopy-pipeline-uj00F`
**Triggering incidents (live boot 2026-05-09 20:57:03 IST):**
1. `[ERROR] PREVOI-01 prev_oi loader: unzip failed — Broken pipe` — macOS Info-ZIP 6.00 stdin pipe failure
2. `[HIGH] TickVault is partially online — 0 of 5 market-data feeds connected (5 stuck)` Telegram during pre-market boot when all 5 conns are correctly DEFERRED waiting for 09:00 IST
3. Operator confusion that `data/logs/movers/` directory still exists post-PR-#539 retirement

## Plan Items

- [x] **1. Strip boot-time bhavcopy step from prev_oi loader** (avoids PREVOI-01 entirely)
  - Files: `crates/app/src/prev_oi_loader.rs`
  - Behaviour change: `load_prev_oi_cache_at_boot_with_overlay` starts with empty `HashMap` and runs ONLY the Option Chain REST overlay. Live boot 20:57:40 IST proved overlay alone produced 866 entries covering NIFTY/BANKNIFTY/SENSEX (= full Wave-5 indices-only scope).
  - Delete: `pub async fn load_prev_oi_cache_at_boot` (no remaining external callers)
  - Delete imports: `unzip_csv_from_zip_body`, `fetch_bhavcopy_zip`, `parse_bhavcopy_csv`, `build_prev_oi_cache_from_bhavcopy`, `BhavcopySegment`
  - Keep: `unzip_csv_from_zip_body` in `bhavcopy_scheduler.rs` (still used by 16:30 IST `bhavcopy_pipeline.rs` cycle, on hold per operator)
  - Keep: `ErrorCode::PrevOi01CacheLoaderUnzipFailed` (remains valid for 16:30 cycle)
  - Tests: `test_load_prev_oi_cache_at_boot_with_overlay_skips_bhavcopy_step` — asserts only the overlay path runs
  - Tests: `test_prev_oi_loader_imports_no_longer_reference_bhavcopy_unzip` — source-scan ratchet

- [x] **2. Off-hours gate on partial-online Telegram** (avoids false `[HIGH]` alarm)
  - Files: `crates/app/src/main.rs` (boot final-snapshot block ~7720-7757)
  - Files: `crates/core/src/notification/events.rs` (add typed variant or dispatch flag)
  - Behaviour change: when `connected != total` AND `!is_within_market_hours_ist()`, emit `WebSocketPoolDeferredOffHours { deferred, total, boot_path }` as Severity::Low instead of `WebSocketPoolPartialAfterDeadline` (Severity::High).
  - Wording (Severity::Low): "✅ Boot complete — {deferred}/{total} main feeds DEFERRED until 09:00 IST (Dhan resets idle pre-market sockets; this is by design)"
  - In-market path unchanged — partial-after-deadline keeps firing High.
  - Tests: `test_partial_pool_off_hours_emits_low_severity_deferred_event`
  - Tests: `test_partial_pool_in_market_hours_keeps_high_severity` (regression block)
  - Tests: `test_deferred_off_hours_event_is_immediate_low_severity` (Telegram dispatch policy)

- [x] **3. Retire `LogCategory::Movers`** (operator confusion + dead category post-#539)
  - Files: `crates/app/src/observability.rs`
  - Delete: `LogCategory::Movers` enum variant + `CATEGORY_MOVERS_DIR` const + match arms in `dir()`, `prefix()`, `build_category_targets()`
  - Merge: 4 tracing targets (`mover_classifier`, `movers_window`, `option_movers`, `top_movers`) into `LogCategory::LiveTicks` target list
  - Update: `LogCategory::iter()` if present, plus any `for cat in LogCategory::iter()` loops at boot
  - Update: existing tests `LogCategory::Movers.dir()` / `.prefix()` / `build_category_targets(LogCategory::Movers)` must be deleted; replace with one regression test asserting the 4 targets land under `LiveTicks`
  - On-disk `data/logs/movers/` directory becomes orphaned; operator removes manually (ratchet does NOT clean filesystem state)
  - Tests: `test_log_category_movers_variant_is_retired_and_targets_live_under_live_ticks`
  - Tests: `test_log_category_iter_no_longer_includes_movers`

## Scenarios

| # | Scenario | Expected |
|---|---|---|
| 1 | Off-hours boot (Sat 20:57 IST) | No `[HIGH]` partial-online Telegram. Single `[LOW] Boot complete — 5/5 deferred until 09:00 IST` instead. PREVOI-01 does NOT fire. |
| 2 | In-market boot, 1 conn fails after deadline | Existing `[HIGH] TickVault partially online — 4/5 connected (1 stuck)` keeps firing — regression test pins this. |
| 3 | Greeks pipeline starts post-#539 | RAM trackers (`option_movers`, `top_movers`) keep emitting; their log lines now route to `data/logs/live_ticks.YYYY-MM-DD.log` instead of a separate movers log file. |
| 4 | 16:30 IST bhavcopy cycle (still on hold) | `unzip_csv_from_zip_body` remains compilable + callable; volume_nse_audit cross-check operator placeholder unchanged. |

## 15-Row + 7-Row Guarantee Matrix

Per `.claude/rules/project/per-wave-guarantee-matrix.md`:

| Demand | Mechanical proof |
|---|---|
| 100% code coverage | Each item ships a unit + a source-scan ratchet; FULL_QA=1 covers integration |
| 100% audit coverage | No new audit table needed; existing boot_audit captures the partial-online state |
| 100% testing | 22 categories — unit + source-scan + Telegram-formatter property tests |
| 100% code checks | banned-pattern + pub-fn-test + plan-verify gates run |
| 100% perf | No hot-path changes |
| 100% monitoring | Counters `tv_telegram_dropped_total`, `tv_websocket_connections_active` already wired |
| 100% logging | All paths use `tracing` macros |
| 100% alerting | Off-hours deferred event = Low (informational only — no Prom alert by design) |
| 100% security | No external input handling changes |
| 100% security hardening | Removes one shell-out (unzip) from boot path = attack-surface reduction |
| 100% bugs fixing | Pre-impl + post-impl 3-agent adversarial review |
| 100% scenarios covering | 4 scenarios above + ratchet tests |
| 100% functionalities covering | Every retired pub fn deletion verified by missing call sites (pub-fn-wiring guard) |
| 100% code review | hot-path + security + general-purpose agents on diff |
| 100% extreme check | All ratchets fail build on regression |

| Resilience demand | Honest envelope |
|---|---|
| Zero ticks lost | Unchanged — no tick-path edit |
| WS never disconnects | Improvement — off-hours deferral is now correctly classified, no false alarm |
| Never slow/locked | Unchanged — no hot-path edit |
| QuestDB never fails | Unchanged |
| O(1) latency | Unchanged |
| Uniqueness + dedup | Unchanged |
| Real-time proof | New `WebSocketPoolDeferredOffHours` event + ratchets pin the wording |

## Honest 100% Claim

100% inside the tested envelope, with ratcheted regression coverage:
- Off-hours deferred main-feed conns no longer trip a HIGH Telegram (test pinned).
- Boot-time PREVOI-01 unzip path retired entirely (deleted code cannot fail).
- `LogCategory::Movers` retired (variant deletion = compile-time guarantee).
Beyond the envelope: 16:30 IST bhavcopy cross-check still depends on macOS unzip; that's on operator hold and not in scope for this plan.
