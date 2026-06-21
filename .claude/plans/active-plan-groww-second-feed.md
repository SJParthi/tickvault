# Implementation Plan: Groww as a pluggable SECOND live feed (reuse WAL/ring/spill; live-vs-backtest 1m parity)

**Status:** APPROVED
**Date:** 2026-06-19
**Approved by:** Parthiban (2026-06-19, AskUserQuestion: "Go — start PR-0 (Groww default OFF)")
**Branch:** `claude/compassionate-faraday-728tnc`
**Authority:** CLAUDE.md > operator-charter-forever.md §I > groww-second-feed-scope-2026-06-19.md > design-first-wall.md > this file
**Crates touched (future PRs):** `common` (config), `core` (groww feed/parser/aggregate/cross-verify), `storage` (groww tables), `app` (boot wiring)

## Design

Add **Groww** as market-data **feed #2** alongside the existing **Dhan** feed #1, per the
operator authorization in `groww-second-feed-scope-2026-06-19.md`. The two feeds are pluggable:
a `[feeds]` config section with `dhan_enabled` (default `true`, unchanged) and `groww_enabled`
(default `false`) lets the operator run Dhan-only, Groww-only, or both. Groww **reuses the
existing resilience architecture AS-IS** — the same WAL frame spill, rescue ring buffer, disk
spill, DLQ, tick processor, and 1-minute aggregator that Dhan uses — never redesigning them.
Groww is a SECOND PRODUCER into those reused blocks, writing to its OWN namespaced QuestDB
tables (`groww_live_ticks`, `groww_candles_1m`, `groww_cross_verify_1m_audit`); the Dhan
`ticks`/`candles_*`/audit tables are untouched.

Why Groww: Dhan has told us its live WebSocket has "minor changes and fluctuations / missed
ticks". Groww gives an INDEPENDENT second source whose tick timestamps carry **millisecond**
precision (Dhan's are whole seconds), enabling cleaner minute-boundary bucketing and exact
ordering. The deliverable is a **Groww-live-1m vs Groww-backtest-1m parity check** (exact OHLCV
compare, minute-by-minute, naming every mismatch) — confirming Groww's live feed agrees with
Groww's own historical/backtest data, giving the operator a trustworthy yardstick to measure
Dhan's drift.

**Native Rust ONLY — brutex is reference, not code (operator lock 2026-06-19, Quote 3):**
NOTHING is pulled, vendored, or imported from the brutex repo. The brutex `live` package is used
purely as a **design/protocol REFERENCE** (the idea + the Groww live-feed wire details); the
implementation is **native tickvault Rust** reusing our existing O(1) chain. No `growwapi` Python
dependency, no Python sidecar, no vendored files. PR-1 implements the Groww live-feed connector
in Rust against Groww's live-feed protocol (token + TOTP auth, implemented natively) feeding raw
frames into the SAME `ws_frame_spill` → ring → spill → DLQ. Groww inherits the SAME bounded
zero-tick-loss capture guarantee as Dhan — WAL-before-broadcast + disconnect/reconnect with
subscription preservation — so not a single RECEIVED tick is dropped inside the tested chaos
envelope, on Groww exactly as on Dhan. Honest envelope: that is a CAPTURE guarantee (every tick
we receive is durable); it does NOT claim Groww's UPSTREAM exchange feed never drops a tick —
measuring that drift vs Dhan is the goal. Default-OFF guarantees zero prod impact until proven.

## Plan Items

- [x] PR-0 — Governance: operator authorization rule file + this approved plan + 2 pointer lines. NO code.
  - Files: `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`, `.claude/plans/active-plan-groww-second-feed.md`, `.claude/rules/project/websocket-connection-scope-lock.md`, `.claude/rules/project/operator-charter-forever.md`
  - Tests: n/a (docs/rules only — plan-gate exempt: no `crates/*/src`)
- [ ] PR-1 — `[feeds]` config (`FeedsConfig` in `crates/common/src/config.rs` + `config/base.toml`, Groww default OFF) + **native Rust** Groww live-feed connector (token + TOTP auth implemented natively; brutex referenced for protocol ONLY, zero code pulled) feeding raw frames into the EXISTING `ws_frame_spill` → ring → spill → DLQ with the same disconnect/reconnect + subscription-preservation. Behind `groww_enabled=false`.
  - Files: `crates/common/src/config.rs`, `config/base.toml`, `crates/core/src/feed/groww/*.rs`, `crates/app/src/main.rs` (additive boot branch, gated)
  - Tests: `test_feeds_config_defaults_groww_off`, `test_groww_feed_only_spawns_when_enabled`, `test_groww_frame_enters_wal_spill`
- [ ] PR-2 — Groww tick parser (ms timestamps) → tick processor → 1-minute aggregator → `groww_candles_1m` QuestDB table (DEDUP UPSERT KEYS).
  - Files: `crates/core/src/feed/groww/parser.rs`, `crates/storage/src/groww_candle_persistence.rs`, `crates/core/src/feed/groww/aggregate.rs`
  - Tests: `test_groww_ms_timestamp_buckets_to_correct_minute`, `test_groww_1m_ohlcv_fold`, `test_groww_candles_dedup_key_includes_segment`
- [ ] PR-3 — Groww backtest/historical 1m fetch + live-vs-backtest EXACT 1m cross-verify → `groww_cross_verify_1m_audit` + Telegram/console parity report (names every mismatched minute; no false-OK on empty set).
  - Files: `crates/core/src/feed/groww/cross_verify.rs`, `crates/storage/src/groww_cross_verify_audit_persistence.rs`, `crates/common/src/error_code.rs` (GROWW-XVERIFY-01/02)
  - Tests: `test_groww_live_vs_backtest_exact_match`, `test_groww_xverify_names_mismatch_minute`, `test_groww_xverify_no_false_ok_on_empty`
- [ ] PR-4 — Runtime per-feed enable/disable wiring + (optional) Dhan-vs-Groww live parity report + dashboards/observability + chaos/hardening tests.
  - Files: `crates/app/src/main.rs`, `crates/core/src/feed/parity.rs`, `crates/storage/tests/chaos_groww_feed_*.rs`
  - Tests: `test_feed_toggle_all_three_modes`, `chaos_groww_spill_saturation`, `test_dhan_vs_groww_live_parity_report`

- [x] PR-4a — **Feed-toggle API endpoint** (operator AskUserQuestion 2026-06-19: "Feed-toggle API endpoint"). True two-way runtime control: an authenticated endpoint flips a shared `Arc<FeedRuntimeState>` (per-feed `AtomicBool`, O(1) lock-free) that the Groww lane checks LIVE — pause/resume Groww writes mid-session with NO restart. The SAME `Arc` is shared by the Axum API + the Groww bridge so a toggle is seen instantly.
  - Files: `crates/api/src/feed_state.rs` (NEW — `Feed` enum + `FeedRuntimeState`), `crates/api/src/handlers/feeds.rs` (NEW — `GET /api/feeds`, `POST /api/feeds/{feed}`), `crates/api/src/handlers/mod.rs`, `crates/api/src/lib.rs` (protected routes), `crates/api/src/state.rs` (`new_with_feed_runtime` + accessor), `crates/app/src/main.rs` (construct from config + inject), `crates/app/src/groww_bridge.rs` (live `is_enabled` check)
  - Tests: `feed_state` unit tests (from_config / is_enabled / set_enabled / snapshot / Feed parse), handler tests (status JSON, toggle flips, bad-feed 400), bridge idles-when-disabled test

- [x] PR-S (sidecar auto-launcher) — **"no manual commands" (operator 2026-06-19):** tickvault auto-provisions an isolated Python venv (one-time `python -m venv` + `pip install growwapi pyotp`, idempotent), fetches the SSM Groww creds, injects them as env, spawns `scripts/groww-sidecar/groww_sidecar.py` from that venv, and supervises it (restart-on-crash with capped exponential backoff; stop/resume on the runtime feed toggle). The operator flips ONE flag (`groww_enabled` config OR `/api/feeds`) and never touches a terminal. Default OFF; touches NO Dhan path. This is the Python-now half of "Both — Python now, Rust later".
  - Files: `crates/app/src/groww_sidecar_supervisor.rs` (NEW), `crates/app/src/lib.rs` (module decl), `crates/app/src/main.rs` (spawn in the `groww_enabled` block)
  - Tests: `test_sidecar_restart_backoff_*` (zero/exponential/capped/never-exceeds-max), `test_venv_python_path_joins_bin_python3`, `test_venv_is_provisioned_*`, `test_build_venv_create_command_parts`, `test_build_pip_install_command_uses_venv_python_and_requirements`, `test_build_sidecar_run_command_runs_script_with_venv_python`, `test_groww_sidecar_options_default_paths`

- [x] PR-BV (bridge tick validation — security follow-up) — **the Groww bridge writes `feed='groww'` rows into the SHARED `ticks`/`candles_1m` tables; it now validates NDJSON fields from the sidecar before persist** (deep security review 2026-06-19, MEDIUM). NEW pure `validate_groww_tick` rejects non-finite/≤0/implausible `ltp` (mirrors Dhan `is_valid_ltp` + `MAX_PLAUSIBLE_LTP`), negative `cum_volume`, ≤0 `security_id`, and `ts_ist_nanos` outside `[~2020, ~2100]` IST-nanos — wired AFTER parse, BEFORE persist + aggregator fold so one bad tick poisons neither a `ticks` row nor a candle. (Bounded per-poll read + read-failure log-level remain a smaller LOW follow-up; the data-integrity MEDIUM is closed here.)
  - Files: `crates/app/src/groww_bridge.rs`
  - Tests: `test_validate_groww_tick_accepts_valid`, `test_validate_groww_tick_rejects_non_finite_ltp`, `test_validate_groww_tick_rejects_non_positive_ltp`, `test_validate_groww_tick_rejects_implausible_ltp`, `test_validate_groww_tick_rejects_negative_volume`, `test_validate_groww_tick_rejects_non_positive_security_id`, `test_validate_groww_tick_rejects_timestamp_out_of_range`

- [x] PR-A (Groww hardening — 4-agent audit fixes 2026-06-20) — **bounded chunked read** of the NDJSON tick file (HIGH: prevents multi-GB `read_to_string` OOM at ~779-instrument scale) via `GROWW_BRIDGE_MAX_READ_BYTES`=4MiB/poll + UTF-8-safe byte-residual line splitting (`drain_complete_prefix`); **`MAX_PLAUSIBLE_VOLUME`** guard (`GrowwTickReject::ImplausibleVolume`); **sidecar token-leak fix** (log `type(exc).__name__`, never `{exc!r}` which can embed the access token). Parity check shelved per operator 2026-06-20.
  - Files: `crates/app/src/groww_bridge.rs`, `scripts/groww-sidecar/groww_sidecar.py`
  - Tests: `test_drain_complete_prefix_splits_complete_lines_keeps_partial`, `test_drain_complete_prefix_no_newline_returns_empty`, `test_drain_complete_prefix_partial_utf8_tail_buffered`, `test_validate_groww_tick_rejects_implausible_volume`

- [ ] PR-B (Groww universe → ~779 subscribed) — **Rust builds the watch-list** (operator decision 2026-06-20): download Groww master `instrument.csv` (`growwapi-assets.groww.in/instruments/instrument.csv`) + the NIFTY-Total-Market list, join NTM ISINs → Groww `exchange_token` (cash stocks) + NSE indices → `exchange_token`, write a watch file the sidecar reads. Sidecar: read watch file (replace hardcoded WATCH), `subscribe_ltp` (stocks) + `subscribe_index_value` (indices, fixes the IDX_I MEDIUM). ~779 instruments.
  - [x] PR-B1 — Rust watch-list builder + boot wiring (merged #1165): parse + ISIN-resolve + dedup + envelope + deterministic cap; `build_and_write_groww_watch` downloads master+NTM, writes `data/groww/groww-watch-<date>.json`.
  - [x] PR-B2 — sidecar reads the watch file → `subscribe_ltp` STOCKS (kind="ltp", numeric token → integer security_id); Option A volume=0; capture-at-receipt NDJSON. Indices counted+logged, deferred to PR-B2i. Files: `scripts/groww-sidecar/groww_sidecar.py`.
  - [ ] PR-B2i — INDICES: `subscribe_index_value` + name→canonical-SID identity map (Groww NSE index token is a NAME, not the bridge's integer security_id). BLOCKED on a live Groww index row (verify token form + the canonical SID alignment to Dhan for cross-feed comparison).


- [ ] PR-C (Groww-only on Mac) — confirm `dhan_enabled=false` + `groww_enabled=true` boot path runs standalone on a local MacBook; creds via AWS SSM (operator decision 2026-06-20: Mac has AWS creds); docs.
  - Files: `config/*.toml`, docs
  - Tests: Groww-only boot-path guard


### Design (PR-4a)
A single `Arc<FeedRuntimeState>` is built at boot from `config.feeds` (`dhan_enabled`/`groww_enabled` seed the atomics) and shared into BOTH the API `AppState` (via `new_with_feed_runtime`, leaving the 4-arg `new` unchanged for the 22 existing call sites) AND `run_groww_bridge`. `GET /api/feeds` returns `{dhan: bool, groww: bool}`; `POST /api/feeds/{feed}` with `{enabled: bool}` flips the atomic, behind the existing bearer-auth `protected_routes`. The Groww bridge loop reads `is_enabled(Feed::Groww)` each iteration and idles (no file read, no writes) when disabled — live pause/resume.

### Edge Cases (PR-4a)
- `POST /api/feeds/dhan` → **400** in this slice (disabling the primary trading feed mid-session is unsafe; Dhan stays config+restart — honest envelope). Groww is the toggleable feed.
- Unknown feed name → 400 with the allowed list. Missing/invalid JSON body → 400.
- Toggle Groww OFF while the bridge is mid-batch → the in-flight batch finishes its flush (data-safe), then the loop idles on the next iteration. Toggle ON → resumes tailing from the persisted file offset (no tick loss, no double-read).
- Unauthenticated request → 401 via existing middleware (the route is in `protected_routes`).

### Failure Modes (PR-4a)
- API down → the flag is unreadable from outside, but the bridge keeps its last in-memory state (fail-safe: stays whatever it was). No crash.
- Atomic is lock-free; no poisoning/deadlock possible. Reads/writes are `Ordering::Relaxed` (a toggle is advisory, not a memory-ordering barrier).
- Disabling Groww does NOT drop the Groww tables or buffered rows — it only stops NEW writes; a pending flush still lands (no data-at-risk).

### Test Plan (PR-4a)
- Scoped to `api` + `app` (no `common` change → no workspace escalation). `feed_state` pure unit tests; axum handler tests via `tower::ServiceExt::oneshot` (status 200 JSON, toggle 200 + state flip, dhan-disable 400, unknown-feed 400, unauth 401); bridge `is_enabled`-gate unit test. 3-agent adversarial review (hot-path + security + hostile) on the diff.

### Rollback (PR-4a)
- Config remains the source of truth at boot; the runtime flag only overrides within a session. `git revert` removes the endpoint + the live-check; the Groww lane falls back to the boot-config gate (Step C). Zero impact on Dhan.

### Observability (PR-4a)
- Each toggle logs `info!(feed, enabled, "feed runtime toggled via API")` + increments `tv_feed_runtime_toggle_total{feed,action}`. `GET /api/feeds` is the live status read. (A `feed_toggle_audit` QuestDB row is a follow-up; the log + counter are the slice-1 signal.)

## Edge Cases
- Groww `groww_enabled=false` (default) ⇒ feed never spawns; boot path byte-identical to today.
- Same `(symbol, exchange)` tick twice ⇒ QuestDB DEDUP UPSERT KEYS collapse it (O(1) amortized).
- Groww ms-timestamp at a minute boundary (e.g. `09:15:59.998`) ⇒ buckets to the 09:15 minute, never bleeds into 09:16.
- Groww feed down while Dhan up (both-mode) ⇒ Dhan lane fully unaffected (separate connection + tables + pipeline instance).
- Groww live set ≠ Groww backtest set for a minute ⇒ reported as `missing`, never silently equal.
- Empty/partial Groww backtest fetch ⇒ parity reports `partial coverage`, never a false "all match" (audit Rule 11).

## Failure Modes
- Groww auth/token/TOTP failure ⇒ Groww lane fails closed + typed error + retry-with-backoff; Dhan lane untouched. (Operator demand: "pull until it fetches successful response" → bounded exponential-backoff retry per the existing Dhan retry ladders, with escalating Telegram — never an unbounded busy-loop / retry storm.)
- Groww WS drop ⇒ reuse the existing reconnect + WAL-spill recovery; frames already captured to WAL before broadcast are replayed on recovery (zero loss inside the chaos envelope).
- QuestDB down ⇒ Groww writes ABSORB via the SAME ring→spill→DLQ as Dhan; no new failure path.
- Groww-default-ON regression ⇒ ratchet `test_feeds_config_defaults_groww_off` fails the build.
- Groww frame written to a Dhan table (table-mixup regression) ⇒ caught by the DEDUP-segment meta-guard + a source-scan asserting Groww writers target `groww_*` only.

## Test Plan
- PR-0: docs/rules only — plan-gate exempt; pre-push banned-pattern + secret-scan + plan-verify must pass.
- PR-1..PR-4: per `testing-scope.md`, scoped to the changed crate; `common` change (PR-1) escalates to `cargo test --workspace`. 22-test categories per `testing.md` for each changed crate; DHAT zero-alloc + Criterion budget on the Groww hot-path (tick parse + 1m fold); chaos test for Groww spill saturation (PR-4); adversarial 3-agent review (hot-path + security + hostile) on every code diff BEFORE and AFTER per `operator-charter-forever.md` §E.
- Real evidence pasted (test counts, bench numbers) per `zero-loss-guarantee-charter.md` §4 — no "should pass".

## Rollback
- PR-0 is additive docs/rules — `git revert <sha>` removes the authorization + plan, zero runtime impact.
- PR-1..PR-4 are gated behind `groww_enabled=false` (default). Disable flag ⇒ Groww never runs ⇒ instant logical rollback with no redeploy. `git revert` of any PR removes that PR's Groww code; the Dhan core is never touched, so revert is always clean and never affects live Dhan trading/data.

## Observability
- Each Groww code path carries the 7-layer telemetry (Prom counter + gauge + tracing span + structured `error!` with `code = ErrorCode::X.code_str()` + Telegram event + `groww_*_audit` table). New typed ErrorCodes (GROWW-XVERIFY-01/02 in PR-3) get a runbook in `groww-second-feed-scope-2026-06-19.md` or a sibling rule file (cross-ref test enforced). The parity report itself is the headline observability deliverable: "Groww live == backtest" or the exact mismatched-minute list.

## Guarantee matrix
The full 15-row + 7-row matrices from `per-wave-guarantee-matrix.md` apply to every code PR
(PR-1..PR-4). PR-0 applicable rows: 100% audit/governance (authorization rule file + this plan),
100% logging/alerting design (typed codes reserved), 100% scenarios (3 run modes + failure modes
above). N/A for PR-0: code coverage / DHAT / bench / hot-path O(1) — reason: docs/rules only,
zero `crates/*/src`. Any "100% guarantee" in this plan or its PRs means exactly
"100% inside the tested envelope, with ratcheted regression coverage" per
`wave-4-shared-preamble.md` §8 and `groww-second-feed-scope-2026-06-19.md` §5 honest envelope —
never the unbounded literal.

## Scenarios
| # | Scenario | Expected |
|---|----------|----------|
| 1 | Prod default (Groww OFF) | Byte-identical Dhan-only behaviour; Groww code dormant |
| 2 | Groww-only mode | Only Groww feed runs → `groww_live_ticks` + `groww_candles_1m` populate; Dhan idle |
| 3 | Both feeds ON | Dhan + Groww in parallel, separate lanes/tables; neither affects the other |
| 4 | Groww live 1m == Groww backtest 1m | Parity report: ✅ "all match (exact, zero tolerance)" |
| 5 | Groww live 1m ≠ backtest at minute X field Y | Parity report names symbol+minute+field+(live,backtest) values |
| 6 | Groww auth fails at boot | Groww lane fails closed + bounded-backoff retry + Telegram; Dhan unaffected |
| 7 | QuestDB outage during both-mode | Both lanes absorb via ring→spill→DLQ; zero loss inside envelope |
