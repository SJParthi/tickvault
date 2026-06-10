# Coverage Gap Report — main @ `a6fe35c4` (CI run #3112, 2026-06-10)

> **Task:** per-crate actual line coverage vs the 100% thresholds in
> `quality/crate-coverage-thresholds.toml`, plus a ranked uncovered-region map.
> **NO code was changed** — this is a research/evidence document only.
> **Style:** operator-charter §G easy-table format. Every number below is REAL
> (measured this session); nothing is estimated.

---

## 0. TL;DR scoreboard (the 10-second read)

| Question | Answer |
|---|---|
| Is any crate actually at 100%? | **NO. Not one.** Best = `common` 99.57%, worst = `app` 63.43% |
| Whole-workspace line coverage | **89.29%** (81,538 of 91,321 lines) |
| Total uncovered lines | **9,783** across **143 files** |
| Why did CI say "all crates meet thresholds"? | **The gate is broken** — it checked ZERO crates and passed vacuously (§2) |
| Biggest single hole | `crates/app/src/main.rs` — **3,168 missed lines (3.4% covered)** — one file = 32% of the entire workspace gap |
| How much of the gap is actually testable? | Roughly half. The rest is binary entry points, operator-approved `TEST-EXEMPT` live-API code, and scope-locked dead code (§4) |

---

## 1. Provenance & evidence chain (no hallucination — every claim has a receipt)

| Item | Value |
|---|---|
| Target | Latest **green** main "Coverage & Perf" job |
| CI run | `27249124316` (run **#3112**), branch `main`, push, conclusion **success**, 2026-06-10T02:30:34Z |
| Coverage & Perf job | id `80469635478`, conclusion **success** (the FIRST green coverage job after the chronic-red fix in #1072/#1073) |
| Head SHA | `a6fe35c417f7c27b90d8fd40108264729ad9c5b1` |
| CI artifact | `coverage-report`, id `7525200479`, 2,599,864 bytes (zip), sha256 `8e8b1e2dc8db1f226109afff48c776039d92c0f8c08cab091b0a68a9aadc165b`, expires 2026-06-24 |
| Artifact download | **BLOCKED** in this remote session: GitHub redirects artifact downloads to `productionresultssa18.blob.core.windows.net`, which returned `HTTP 403, x-deny-reason: host_not_allowed` (session network allowlist). Not a GitHub auth issue. |
| Fallback used | **Byte-faithful local reproduction at the IDENTICAL SHA**: local HEAD == `a6fe35c4` == CI head SHA (verified via `git rev-parse origin/main`). Same command CI runs: `cargo llvm-cov --workspace --no-fail-fast --json --output-path target/llvm-cov/coverage.json -- --skip dhat_` on rustc 1.95.0 (the pinned toolchain). |
| Local run quality | **7,494 tests passed; 0 failed; 12 ignored** across 188 suites (from `/tmp/llvm-cov-run2.log` `test result:` lines). Zero failures ⇒ coverage numbers are not depressed by broken tests. |
| One deviation from CI | `CARGO_PROFILE_DEV_DEBUG=0` (debuginfo stripped) because the instrumented build with full debuginfo filled this container's ~29 GB disk quota. LLVM source-based coverage uses its own coverage mapping, NOT DWARF debuginfo, so **line-coverage results are unaffected**; only binary size differs. |
| Honest limit | The CI artifact's bytes were never read (network-blocked). These numbers come from the reproduction at the same SHA/command/toolchain. When the artifact is downloadable (e.g. local Mac session), cross-check against sha256 `8e8b1e2d…`. |

---

## 2. 🆘 GAP #0 (P0, ranked above every code gap): the coverage gate enforces NOTHING

> **Juice-shop version:** the shop hired a guard to check every customer's
> receipt, but the guard only watches a door that doesn't exist. Every customer
> walks past unchecked and the guard reports "all receipts verified!" nightly.

| What | Evidence |
|---|---|
| The bug | `scripts/coverage-gate.sh:77` — `re.match(r'crates/([^/]+)/', filename)` is **anchored to start-of-string**, but cargo-llvm-cov writes **absolute** paths (`/home/runner/work/tickvault/tickvault/crates/...`). Zero files match → `crate_lines` is empty → loop prints zero rows → script exits 0. |
| CI proof | Run #3112 gate step log: prints the banner, then **zero per-crate rows**, then "All crates meet coverage thresholds." (gate ran for <1s) |
| Synthetic proof (this session) | Fed the gate a 2-file JSON: relative-path file at 50% → correctly printed `FAIL: core 50.0%`, exit 1. Absolute-path file at 50% → **silently ignored, no row**. |
| End-to-end proof (this session) | Ran the real gate against the real 18.3 MB `coverage.json` (193 files, all absolute paths): zero rows printed, `gate-exit=0` — while the true numbers below show **every crate failing**. |
| Consequence | The "100% minimum for ALL crates" policy (CLAUDE.md CI/CD §5, `crate-coverage-thresholds.toml`) has **never been enforced** on any run that produced absolute paths — i.e. every run. The policy's only green signal is vacuous. |
| Exact fix (for a future approved PR — NOT done here) | (a) change `re.match` → `re.search` (or strip a detected repo-root prefix); (b) **fail-closed**: `if not crate_lines: sys.exit(1)` with "gate matched 0 files — refusing vacuous pass" (audit-findings Rule 11, no false-OK); (c) regression test feeding the gate an absolute-path fixture and asserting non-vacuous output. |
| Exact test that would cover it | `tests/coverage_gate_selftest.sh` (or a `crates/storage/tests/coverage_gate_guard.rs` source-scan ratchet): run `scripts/coverage-gate.sh` against a fixture JSON with absolute paths + a below-threshold crate, assert exit 1 AND ≥1 per-crate row printed; second fixture with zero matching files, assert exit 1 (fail-closed). |

---

## 3. Per-crate scoreboard — actual vs the 100% threshold

Source: local reproduction `coverage.json` (193 files), aggregated exactly the
way the gate is *supposed* to aggregate (sum of line `count`/`covered` per
`crates/<name>/`).

| Crate | Threshold | **Actual** | Verdict | Lines total | Lines covered | **Lines missed** | Files with gaps |
|---|---|---|---|---|---|---|---|
| `common` | 100.0% | **99.57%** | ❌ FAIL (−0.43) | 9,900 | 9,857 | 43 | 8 |
| `api` | 100.0% | **98.71%** | ❌ FAIL (−1.29) | 3,185 | 3,144 | 41 | 6 |
| `trading` | 100.0% | **97.02%** | ❌ FAIL (−2.98) | 18,683 | 18,127 | 556 | 22 |
| `storage` | 100.0% | **91.35%** | ❌ FAIL (−8.65) | 12,420 | 11,346 | 1,074 | 22 |
| `core` | 100.0% | **90.38%** | ❌ FAIL (−9.62) | 34,022 | 30,748 | 3,274 | 64 |
| `app` | 100.0% | **63.43%** | ❌ FAIL (−36.57) | 13,111 | 8,316 | **4,795** | 21 |
| **Workspace** | — | **89.29%** | — | 91,321 | 81,538 | **9,783** | 143 |

**One-glance meaning:** the `app` crate (boot sequence, binaries, orchestrators)
holds **49% of the entire workspace gap**. The hot-path crates (`trading`,
`common`) are genuinely close to the bar; the gap lives in boot/IO/glue code.

---

## 4. WHY the lines are uncovered — the 4 honest categories

Every one of the 9,783 missed lines falls into exactly one of these buckets:

| Cat | Name | What it is | Coverable? | ~Share of gap |
|---|---|---|---|---|
| **A** | Binary entry points & boot orchestration | `main()`, `tv_doctor`, `smoke_test`, boot Steps 1–15 wiring, loaders that need QuestDB/SSM/Docker live | Only via process-level e2e with stub services, or by extracting pure functions | ~45% (the `app` crate + `infra.rs`) |
| **B** | `TEST-EXEMPT` live-network I/O | Functions explicitly marked `// TEST-EXEMPT: requires live/sandbox Dhan API` (28 markers in `api_client.rs` alone, 7 in `instance_lock.rs`) + WS connect/read loops + REST clients + downloaders | Yes — with a local mock HTTP/WS server. **Caveat:** a mock-server dev-dependency (e.g. wiremock) is a NEW dep → needs operator dated approval per CLAUDE.md | ~25% |
| **C** | Scope-locked DEAD code | Planner branches behind `should_subscribe_index_derivatives()`/`should_subscribe_stock_derivatives()` which are `const fn → false` (websocket-connection-scope-lock, PR #7b). These lines are **mechanically unreachable** — no test can ever execute them without violating the operator lock | **NO** — the honest fixes are delete-the-code (operator approval) or accept permanent exclusion | ~5% |
| **D** | Reachable error/edge branches | ILP flush failures, malformed-input arms, rarely-fired Telegram event formatting, watchdog edge cases | **YES — pure unit tests, no new deps.** This is the real actionable backlog | ~25% |

---

## 5. RANKED gap table — top 20 files (≈78% of all missed lines)

Ranked by missed lines. "Exact test" names the concrete test to write; category
per §4.

| Rank | File | Missed | Cov % | Cat | Why uncovered | Exact test that would cover it |
|---|---|---|---|---|---|---|
| 1 | `crates/app/src/main.rs` (ranges 123–7218, 800+ ranges) | **3,168** | 3.4% | A | The 15-step boot binary. Tests never execute `main()`; every step (auth, DDL join, WS spawn, schedulers) requires live SSM/QuestDB/Dhan. Only the small pure helpers (e.g. `trading_day_gate_exit_code`) are covered. | Highest-value slice first: `crates/app/tests/trading_day_gate_cli.rs` using `assert_cmd`-style spawn of the binary with `--check-trading-day` against a fixture config, asserting exit codes 0/75/70 (covers `run_trading_day_gate`, lines 123–157). For the rest: extract step bodies into `boot_steps.rs` pure/injectable functions and unit-test each — `test_boot_step_6c_universe_orchestrator_with_stub_fetcher`, etc. Full `main()` coverage is NOT realistically attainable; recommend a documented exclusion + e2e smoke in CI instead. |
| 2 | `crates/core/src/instrument/subscription_planner.rs` (393–409, 447–691, …) | 456 | 75.5% | **C** | Index/stock-derivative subscription branches gated behind `should_subscribe_index_derivatives()` / `should_subscribe_stock_derivatives()` — both `const fn { false }` (lines 236–247) per the 2-WS scope lock. **Unreachable by design.** | **None possible under the lock.** Options: (a) delete the dead branches in an operator-approved PR (the scope-lock rule file already forbids re-enabling); (b) add the file ranges to a documented coverage-exclusion list. A test forcing these paths would violate `websocket-connection-scope-lock.md`. |
| 3 | `crates/app/src/infra.rs` (123–166, 300–415, …) | 381 | 66.6% | A/B | Docker-compose env wiring with live SSM secrets (`expose_secret` at 149–166), `docker compose up` shell-outs, QuestDB readiness polling against a live socket. | `test_compose_env_vars_built_from_creds` — refactor env-var assembly into a pure `fn build_compose_env(creds) -> [(­&str, String)]` and assert the 4 tuples; `test_wait_for_questdb_ready_times_out_against_closed_port` (tokio test binding an unused localhost port — no Docker needed, covers the BOOT-01/BOOT-02 escalation ladder lines 300–415). |
| 4 | `crates/trading/src/oms/api_client.rs` (593–639, 653–697, 710–750, …) | 326 | 92.0% | B | Conditional-trigger / forever-order / slicing REST methods, each marked `TEST-EXEMPT: requires live/sandbox Dhan API` (28 markers). The send/status/parse chains never execute. | `crates/trading/tests/api_client_mock_server.rs` — spin a local `axum` test server (axum is ALREADY a workspace dep — no new dep needed) returning canned Dhan JSON + DH-9xx error bodies; one test per exempt method: `test_create_conditional_trigger_2xx_parses`, `test_modify_conditional_trigger_dh904_maps_to_rate_limit_error`, etc. |
| 5 | `crates/core/src/notification/events.rs` (1223–1900s formatting arms) | 296 | 86.3% | D | Telegram message-formatting arms for rarely-fired event variants (failure-path events that never fire in tests). Pure string code — fully testable today. | `test_telegram_format_<variant>` unit tests instantiating each un-exercised `NotificationEvent` variant and asserting the formatted body (also enforces the 10 Telegram commandments per variant). Zero deps, highest ROI in the workspace. |
| 6 | `crates/core/src/websocket/order_update_connection.rs` (63–340s) | 262 | 69.5% | B | `run_order_update_connection` connect/auth/read loop — needs a live `wss://` endpoint; reconnect/backoff arms only run on real socket errors. | `crates/core/tests/order_update_ws_local_server.rs` — local `tokio-tungstenite` server (already a workspace dep) that accepts, asserts the MsgCode-42 auth JSON, streams one `order_alert`, then RSTs: covers connect + auth + parse + reconnect arms. |
| 7 | `crates/core/src/pipeline/tick_processor.rs` (1074–1372 etc.) | 249 | 91.3% | D | Error/edge arms: channel-closed handling, malformed packet skips, lag arms, persist-failure escalations. Reachable with crafted inputs. | Extend `crates/core/tests/parser_pipeline.rs`: `test_tick_processor_survives_closed_persist_channel`, `test_unknown_response_code_logs_and_skips`, `test_lagged_broadcast_arm_increments_counter` (drop the receiver / pre-fill the channel to force each arm). |
| 8 | `crates/core/src/option_chain/snapshot_scheduler.rs` (261–543) | 239 | 68.0% | A/B | 50s-cycle scheduler loop: time-gated, calls live REST, mutex skip-next logic only runs in-loop. | Extract the per-cycle decision into the existing pure helpers and add `test_cycle_overlap_skips_next_window`, `test_stale_cache_flags_strategy_halt` with injected clock; loop wiring via a 2-tick run against the §5-style local mock server. |
| 9 | `crates/core/src/websocket/connection.rs` (472–503, 891–958, 1045–1185) | 219 | 91.6% | B | Main-feed read-loop error arms, TLS/connect failures, reconnect ladder — live-socket-only paths. | Same local-WS-server harness as rank 6: `test_main_feed_reconnect_after_server_rst_preserves_subscribe_rx` (also re-pins the PR #337 `SubscribeRxGuard` contract end-to-end). |
| 10 | `crates/core/src/instance_lock.rs` (171–476) | 191 | 50.1% | B | `try_acquire_instance_lock` against real AWS SSM — file itself documents `TEST-EXEMPT: requires a real AWS SSM endpoint`; pure helpers (path/JSON/staleness) ARE covered. | The aws-sdk-ssm client can't be trivially mocked without a new dep. Honest options: keep TEST-EXEMPT (documented), or route through the existing `SecretManager` trait seam and add `test_acquire_lock_already_held_refuses_start` with a stub impl. |
| 11 | `crates/core/src/option_chain/client.rs` (69–337) | 172 | 55.9% | B | Option-chain REST POST + expiry-list fetch + DH-904 ladder — live Dhan only. | Same axum local-mock pattern as rank 4: `test_option_chain_fetch_parses_oc_map_decimal_strikes`, `test_expiry_list_fetch_rejects_missing_client_id_header`, `test_dh904_ladder_exhaustion_emits_option_chain_02`. |
| 12 | `crates/app/src/cross_verify_1m_boot.rs` (444–679) | 169 | 69.3% | A/B | 15:31 IST cross-verify: live Dhan intraday REST + QuestDB reads + CSV write chain. | Pure-function slice is already partly covered; add `test_compare_minutes_exact_match_flags_each_field` (pure, no IO) for 444–556, and `test_csv_writer_emits_header_and_rows` against a tempdir for 622–660. |
| 13 | `crates/app/src/bin/tv_doctor.rs` (entire file) | 168 | **0.0%** | A | Standalone diagnostic binary — no test ever invokes it; `collect_*` shells out to systemd/docker. | `crates/app/tests/tv_doctor_cli.rs`: spawn the built binary with `--format json --journal-lines 1`, assert it exits 0 and emits parseable JSON even when systemd/docker/metrics are absent (the degrade-gracefully contract is exactly what matters on AWS). |
| 14 | `crates/storage/src/tick_persistence.rs` (scattered error arms) | 165 | 97.0% | D | ILP sender error arms, spill-engage branches, flush-failure escalation — only run when QuestDB misbehaves. | Extend the existing chaos suite: `chaos_zero_tick_loss.rs` already proves the happy ring→spill chain; add `test_flush_error_arm_logs_error_with_code` using the existing `new_disconnected()` constructor to force each `Err` arm. |
| 15 | `crates/storage/src/instrument_lifecycle_persistence.rs` (363–833) | 152 | 84.7% | D | UPSERT/audit error arms + rarely-hit transition kinds (`split`, `segment_moved`). | `test_lifecycle_upsert_ilp_error_arm` via `new_disconnected()`; `test_transition_kind_split_row_payload` (pure row-building, no QuestDB). |
| 16 | `crates/app/src/trading_pipeline.rs` (322–644) | 135 | 94.7% | A | Channel-wiring arms that need the full boot context (real WS + aggregator + writer tasks). | `test_pipeline_wiring_with_stub_channels` — construct with bounded stub channels, send 1 synthetic tick end-to-end, assert it reaches the seal-writer stub (no network). |
| 17 | `crates/storage/src/partition_manager.rs` (91–289) | 128 | 47.8% | B/D | S3 archive + partition-detach flows: live QuestDB + AWS S3. | Pure parts: `test_partition_age_selector_picks_only_gt_90d` (pure date math, lines 91–145). S3 upload arms: stub the existing client seam or keep TEST-EXEMPT with the STORAGE-GAP-04 runbook as compensating control. |
| 18 | `crates/storage/src/shadow_persistence.rs` (118–516) | 116 | 51.3% | D | Ring→spill→DLQ drop-class arms (AGGREGATOR-DROP-01 chain) — only run under cascading failure. | `test_handle_drop_all_three_tiers_full_emits_aggregator_drop_01` — construct writer with 1-slot ring + read-only spill dir (tempdir chmod) to force the full cascade deterministically. |
| 19 | `crates/core/src/websocket/connection_pool.rs` (367–596) | 114 | 90.5% | B | Pool-supervisor respawn arms (WS-GAP-05) + watchdog edge arms — need dying tasks on live sockets. | `test_supervisor_respawns_task_that_panics` — spawn a deliberately-panicking stub connection future, assert respawn within the 5s tick + `tv_ws_pool_respawn_total` increment. |
| 20 | `crates/core/src/network/ip_verifier.rs` (70–216, 327–446) | 113 | 84.4% | B | Dhan `getIP`/`setIP` REST + 30×60s retry ladder — live API + static-IP account state. | Same axum mock pattern: `test_get_ip_orders_allowed_false_halts_boot`, `test_set_ip_retry_ladder_caps_at_30_attempts` (injected clock/attempt counter). |

Remaining 123 files (2,162 missed lines, ranks 21–143): full list with exact
line ranges in **Appendix A**. Their causes follow the same four categories —
dominant pattern per crate: `app` tail = boot loaders (Cat A), `core` tail =
parser/watchdog error arms (Cat D, all unit-testable), `storage` tail = ILP
error arms via `new_disconnected()` (Cat D), `trading` tail = OMS/strategy
edge arms (Cat D), `common`/`api` tails = tiny Display/serde branches (Cat D).

---

## 6. Ranked action plan (what to do, in order, when implementation is approved)

| Priority | Action | Effort | Coverage recovered | Needs new dep? |
|---|---|---|---|---|
| **P0** | Fix the vacuous gate (§2) + fail-closed + fixture regression test | ~20 lines | 0% directly — but makes EVERY future number real | No |
| **P1** | Decide `app`-crate policy: extract-and-test boot steps, or set an honest per-crate threshold/exclusion for binaries instead of a fake 100% | design decision | up to +5.2 pp workspace | No |
| **P2** | Cat-D unit tests (events.rs formatting, tick_processor arms, storage error arms via `new_disconnected()`, parser/watchdog branches) | mechanical, no infra | ~+2.7 pp workspace | No |
| **P3** | Local mock-server harness (axum + tokio-tungstenite, both already pinned workspace deps) for api_client / option_chain / WS loops / ip_verifier | medium | ~+1.5 pp workspace | **No** (reuse pinned deps) |
| **P4** | Operator decision on Cat-C dead code in subscription_planner: delete (preferred — the scope lock already forbids re-enabling) or document exclusion | operator call | +0.5 pp | No |
| **P5** | Re-run Coverage & Perf; the FIXED gate now prints 6 real rows; ratchet thresholds upward from the real baseline (e.g. `app = 64.0` today, raise as P1–P4 land) instead of a fictional 100 | config | honesty | No |

---

## 7. Honest envelope (operator-charter §F wording)

These numbers are 100% real **inside this envelope**: measured at SHA
`a6fe35c4` with the CI-identical command and pinned rustc 1.95.0, 7,494/7,494
tests passing, debuginfo stripped (provably irrelevant to LLVM coverage
mapping); the CI artifact itself (sha256 `8e8b1e2d…`) was network-blocked and
should be diff-checked from a network-unrestricted session. Coverage of
`#[ignore]`d tests (12) and `dhat_*` tests (skipped, exactly as CI skips them)
is excluded — same as CI. No number in this file is estimated, extrapolated,
or copied from a previous claim.

---

## Appendix A — every file with a gap (all 143, ranked, with uncovered line ranges)

| # | Missed | Cov % | File | Uncovered line ranges |
|---|---|---|---|---|
| 1 | 3168 | 3.4% | `crates/app/src/main.rs` | 123–127, 129, 131, 133–135, 137–141, 146, 155–157, 165–167, 177–185, 187, 189–191, 199–203, 207–211, 225–226, 231, 235–237, 243–245, 249–251, 259, 262–266, 268–270, 273, 275–276, 280–286, 294–304 (+786 more ranges) |
| 2 | 456 | 75.5% | `crates/core/src/instrument/subscription_planner.rs` | 314, 334, 393–395, 399–409, 447, 455, 462–467, 469–470, 474–475, 482, 485–488, 490, 492–501, 507–508, 510–512, 515–517, 519–525, 532, 536, 540, 544–559, 563–564, 567, 569, 571 (+83 more ranges) |
| 3 | 381 | 66.6% | `crates/app/src/infra.rs` | 123, 126–128, 130–133, 137–139, 143–144, 149–166, 172–173, 177, 180–183, 185, 189, 192–194, 196, 200–201, 203–204, 210–212, 216–217, 221–222, 225, 228, 239, 300–303, 307–311, 315, 336 (+106 more ranges) |
| 4 | 326 | 92.0% | `crates/trading/src/oms/api_client.rs` | 64, 593, 595, 599, 601–605, 607, 612–639, 653, 655, 659, 661–665, 667, 672–697, 710, 716, 718–722, 724, 739, 742–750, 768, 773, 775–779, 796, 801, 803–807 (+199 more ranges) |
| 5 | 296 | 86.3% | `crates/core/src/notification/events.rs` | 186–189, 191, 1223, 1270, 1272, 1290, 1322, 1358, 1382, 1449–1450, 1452–1453, 1455, 1457, 1463–1464, 1495, 1542–1543, 1548–1549, 1553–1554, 1576–1580, 1582–1583, 1600, 1639–1640, 1646–1647, 1709–1711, 1713 (+80 more ranges) |
| 6 | 262 | 69.5% | `crates/core/src/websocket/order_update_connection.rs` | 63–75, 77, 84–88, 96, 98–108, 110, 114–115, 117–120, 122, 124, 127, 130, 137, 139–140, 144–149, 152, 154, 157, 162–163, 168, 180–183, 202–211, 215, 218–229, 238–243 (+56 more ranges) |
| 7 | 249 | 91.3% | `crates/core/src/pipeline/tick_processor.rs` | 309, 396, 792–795, 798, 800–801, 805, 863, 868, 873–874, 877, 879–881, 885–886, 889, 898, 974, 982, 1003–1008, 1031, 1046, 1074–1082, 1094–1098, 1130–1136, 1159–1161, 1177, 1183–1185 (+64 more ranges) |
| 8 | 239 | 68.0% | `crates/core/src/option_chain/snapshot_scheduler.rs` | 261–274, 277–278, 280–281, 286–297, 302–310, 314–316, 318, 321–322, 326–327, 332–333, 337, 340–343, 348–349, 351, 356, 360–371, 380–384, 389–390, 408–420, 429–433, 438–442, 445–447, 449–451, 453, 455–458 (+47 more ranges) |
| 9 | 219 | 91.6% | `crates/core/src/websocket/connection.rs` | 197, 472–475, 484–489, 497–503, 548–550, 891, 905–910, 912–913, 915–916, 918–919, 926–928, 938, 940–947, 950–952, 955, 957–958, 964, 969, 1045–1052, 1082–1083, 1088, 1090, 1093, 1095–1096, 1098 (+71 more ranges) |
| 10 | 191 | 50.1% | `crates/core/src/instance_lock.rs` | 171–176, 186–194, 199, 201, 208, 210–216, 221–222, 227–229, 231–232, 239–241, 246–248, 253, 256–261, 267, 269–272, 276, 283–285, 288, 305–310, 312, 314–316, 318–334, 343–349, 354, 356–359 (+24 more ranges) |
| 11 | 172 | 55.9% | `crates/core/src/option_chain/client.rs` | 69–73, 75–82, 88–93, 95–99, 101, 103–113, 115–120, 126–130, 134–140, 142, 145–146, 152–156, 160–162, 165–168, 179–184, 189–191, 194–195, 201–207, 209–214, 216, 218–228, 230–235, 241–245, 249–255, 258 (+10 more ranges) |
| 12 | 169 | 69.3% | `crates/app/src/cross_verify_1m_boot.rs` | 260–265, 349, 377, 381, 392, 396, 444–454, 456–459, 461–463, 466–469, 474–475, 479–482, 484–490, 493–494, 496–498, 500–501, 503–504, 507–510, 512–515, 520–524, 526–531, 533–538, 542–556, 560–562, 566–568 (+12 more ranges) |
| 13 | 168 | 0.0% | `crates/app/src/bin/tv_doctor.rs` | 45–52, 54–61, 63–66, 68, 70–78, 80–81, 83–87, 89–90, 98–101, 103–113, 115–118, 120–130, 132, 135–136, 138–142, 144, 146, 148–164, 179–208, 210–222, 224, 226–238, 240, 242–244 |
| 14 | 165 | 97.0% | `crates/storage/src/tick_persistence.rs` | 418, 471–473, 479, 481, 483–484, 486–487, 489, 493, 495, 509, 537, 572, 574–576, 578–581, 583–587, 589–590, 609, 683, 707–708, 713–715, 734–735, 782–786, 800–805, 808–809, 837 (+139 more ranges) |
| 15 | 152 | 84.7% | `crates/storage/src/instrument_lifecycle_persistence.rs` | 363–364, 368–371, 373, 402–406, 408–409, 414–415, 417, 421–422, 429, 505, 511, 516–517, 520–529, 556, 561, 565–567, 571, 574, 577, 579–580, 583, 586–592, 596, 600, 604–605 (+38 more ranges) |
| 16 | 135 | 94.7% | `crates/app/src/trading_pipeline.rs` | 234, 255–257, 283, 285–286, 289–294, 322, 330–331, 333–334, 336–350, 355, 359–360, 368–369, 382, 390–393, 395–409, 414, 418–419, 427–428, 442, 449–450, 452–454, 459, 464–465, 467, 469–470 (+34 more ranges) |
| 17 | 128 | 47.8% | `crates/storage/src/partition_manager.rs` | 91–95, 97–102, 111–115, 117–118, 120, 122–123, 130–136, 138–142, 144–145, 151–157, 159–163, 165–166, 176–182, 184–188, 190–191, 196, 198–199, 205–210, 212, 219–225, 227–238, 240–243, 245, 247–249, 251–253 (+5 more ranges) |
| 18 | 116 | 51.3% | `crates/storage/src/shadow_persistence.rs` | 118, 147–148, 152–155, 157, 164–167, 171–172, 191, 198–200, 204–206, 211–216, 218, 228–234, 236–237, 240, 242, 244, 361, 364–366, 370–371, 373, 377–380, 394–396, 400–402, 406–412, 415–418 (+17 more ranges) |
| 19 | 114 | 90.5% | `crates/core/src/websocket/connection_pool.rs` | 37–40, 42–43, 243–245, 252, 281–283, 324, 367–372, 375–378, 389–392, 401–405, 409, 411–412, 416–417, 422–423, 425–429, 431, 433, 437–438, 442, 445–446, 448, 452–453, 457, 460–463, 467–468 (+28 more ranges) |
| 20 | 113 | 84.4% | `crates/core/src/network/ip_verifier.rs` | 70–73, 76–79, 110, 116–117, 119–120, 122, 135–136, 139–143, 150–152, 157, 160–164, 171–173, 178–182, 187–191, 193–197, 199–201, 203–206, 208, 211–213, 215–216, 327–331, 347–348, 360–362, 370–374, 392–393 (+14 more ranges) |
| 21 | 105 | 93.6% | `crates/core/src/auth/token_manager.rs` | 166–167, 342–349, 394–395, 414, 416–420, 424–425, 436, 457, 472–474, 484–488, 509, 511, 513, 538–540, 552, 630, 804, 841, 862–867, 869, 871, 878, 883–886, 897, 906–911 (+27 more ranges) |
| 22 | 102 | 73.3% | `crates/app/src/prev_day_ohlcv_boot.rs` | 97, 147–150, 172–176, 180, 243, 247, 268, 270, 377–384, 388, 390–393, 395–396, 404, 406–409, 411–413, 417, 420–422, 424–432, 436–440, 443–447, 449–464, 466–472, 477–480, 487–488, 492–511 (+1 more ranges) |
| 23 | 91 | 92.1% | `crates/core/src/notification/service.rs` | 263, 265, 275, 334–335, 370–373, 375, 378–380, 414, 424, 438, 442, 456, 508–511, 515–520, 522–524, 527, 529–534, 548, 552, 554, 557, 566–573, 577–584, 586, 588–592 (+18 more ranges) |
| 24 | 85 | 89.7% | `crates/app/src/boot_helpers.rs` | 59, 63, 112–115, 307–309, 325–328, 331–340, 344–346, 362–369, 372, 374–376, 378, 380–384, 387, 389–391, 394, 397, 400, 403, 451–452, 456–457, 471–473, 480, 511, 522, 526 (+21 more ranges) |
| 25 | 79 | 89.9% | `crates/app/src/observability.rs` | 252, 254, 294, 307, 331–332, 334, 336, 342, 372, 378, 402–403, 405, 407, 413, 434–435, 437, 440–443, 447–449, 451–453, 455–457, 459–462, 465, 486, 492, 509 (+36 more ranges) |
| 26 | 78 | 0.0% | `crates/app/src/bin/smoke_test.rs` | 48–51, 56–64, 69–76, 81–88, 96–105, 110–115, 124–130, 132–134, 136–137, 139, 141–142, 144–153, 155–156 |
| 27 | 69 | 88.2% | `crates/storage/src/ws_frame_spill.rs` | 57, 196–198, 202, 206, 210, 223, 277–280, 283, 285, 287, 294, 297, 299–300, 310–312, 394–400, 403–407, 420, 464–465, 467–469, 474–476, 481–482, 526–528, 537–542, 612, 625–627 (+15 more ranges) |
| 28 | 68 | 81.3% | `crates/storage/src/instrument_fetch_audit_persistence.rs` | 298–299, 303–306, 308, 325–329, 331–332, 337–338, 340, 344–345, 352, 416, 422, 427–428, 432–441, 452–453, 466–469, 473–476, 480–499, 609–613 |
| 29 | 68 | 83.6% | `crates/app/src/bar_cache_loader.rs` | 141–145, 149–183, 209, 213–217, 227, 237, 242, 248, 254, 263, 336, 343, 350, 357, 364, 367, 371, 374, 377, 380, 383, 386, 389, 392 |
| 30 | 62 | 65.2% | `crates/app/src/index_constituency_boot.rs` | 102–107, 109–112, 116, 119–121, 125–126, 130–133, 137, 140–144, 147–151, 156–160, 165–179, 182–183, 185–188, 192–193, 195, 200 |
| 31 | 60 | 61.8% | `crates/app/src/prev_oi_loader.rs` | 80, 140–145, 148–154, 156–157, 163, 167–168, 170, 174, 177–179, 181–184, 190, 193–194, 196, 200, 205–207, 209–212, 219, 224–227, 229–232, 234–235, 237, 247, 250–251, 255–256, 365 (+1 more ranges) |
| 32 | 59 | 50.4% | `crates/core/src/option_chain/expiry_warmup.rs` | 99–107, 109–113, 116–118, 120–121, 127, 129, 140–146, 148–151, 153–157, 162–164, 166–168, 172–179, 186, 192–193, 197 |
| 33 | 57 | 74.1% | `crates/storage/src/prev_day_ohlcv_persistence.rs` | 128–129, 133–135, 137–139, 143, 146–151, 153–157, 160, 167–171, 173–177, 180, 182, 206–215, 217–218, 222–226, 229, 260, 262, 264, 266, 268, 270, 272, 274, 276, 278 (+2 more ranges) |
| 34 | 52 | 77.4% | `crates/core/src/pipeline/prev_oi_cache.rs` | 168, 174–178, 190–192, 194–199, 201–203, 211–212, 214, 218–221, 231–233, 235, 241–246, 249–251, 256–262, 264, 266, 275, 277, 279–280, 310, 488, 496, 511 |
| 35 | 51 | 61.7% | `crates/storage/src/index_constituency_persistence.rs` | 114–115, 119–121, 123–126, 129, 143–147, 149–150, 155–156, 158, 162–163, 166, 178, 180, 184–186, 189, 192–195, 209–220, 222–233 |
| 36 | 51 | 78.9% | `crates/core/src/pipeline/tick_enricher.rs` | 199–202, 205, 207–208, 211–212, 218–221, 223, 227, 234, 288, 292, 295–298, 300, 303–304, 307, 311–313, 315–318, 320–329, 333, 337–338, 340–341, 346, 350 |
| 37 | 50 | 60.3% | `crates/core/src/instrument/index_constituency/downloader.rs` | 123, 140–152, 154–159, 161–163, 167–169, 185–187, 201–209, 211–217, 219–220, 269 |
| 38 | 49 | 71.7% | `crates/app/src/option_chain_cache_loader.rs` | 114–118, 122–142, 145–151, 178–179, 246–250, 256, 263–264, 269, 271–272, 279 |
| 39 | 47 | 89.1% | `crates/core/src/notification/summary_writer.rs` | 131–132, 143–145, 147, 161, 173–174, 176, 179, 184, 187, 204, 221–223, 244, 249, 316, 374, 402, 404, 407, 430, 449, 535, 537, 541, 551, 563 (+14 more ranges) |
| 40 | 46 | 78.0% | `crates/storage/src/cross_verify_1m_audit_persistence.rs` | 168–169, 173–175, 177–179, 183, 186–191, 193–197, 200, 202, 219–228, 230–231, 235–239, 242, 279, 281, 283, 285, 290, 292, 294, 296, 298, 305, 309–313 |
| 41 | 46 | 74.4% | `crates/app/src/lifecycle_reconcile_orchestrator.rs` | 155, 160–161, 166–167, 176–178, 180–185, 189–198, 200–211, 213–222, 232 |
| 42 | 44 | 72.7% | `crates/core/src/instrument/index_constituency/mod.rs` | 68–74, 76–83, 91–92, 96, 117, 150–157, 159–163, 165, 169, 171–173, 176–178, 183–184 |
| 43 | 40 | 91.5% | `crates/core/src/auth/secret_manager.rs` | 65, 69, 97–99, 112, 123–128, 130–135, 137, 202–203, 233, 620, 645, 676, 746, 780, 818, 863, 882, 913, 936, 963–964, 1076, 1128, 1138, 1155 |
| 44 | 39 | 98.2% | `crates/trading/src/oms/engine.rs` | 155–157, 161–163, 410–411, 423, 429, 470, 497–498, 505, 511, 536, 622–623, 653–654, 673, 677, 689, 1102, 1431, 1466, 1544, 1565, 1611, 1632, 1695, 1768, 1803 (+22 more ranges) |
| 45 | 39 | 23.5% | `crates/app/src/day_ohlc_orchestrator.rs` | 52–57, 59–63, 65–69, 71–73, 76–77, 81–82, 94–95, 97–98, 102, 106–107, 109, 111, 115, 117–119, 124 |
| 46 | 36 | 79.5% | `crates/storage/src/disk_health_watcher.rs` | 68–71, 80–82, 90, 94–95, 104, 106, 111–112, 130, 137–138, 140, 150, 152–153, 155–159, 167–168, 175–178, 205, 235–237, 239, 241, 245–246, 322–323, 358, 360–361 |
| 47 | 36 | 95.4% | `crates/core/src/network/ip_monitor.rs` | 140, 161–162, 169–174, 180–181, 209, 221, 236, 275, 286, 294, 412, 529, 599, 644–648, 682, 714, 738, 782, 817, 828, 916, 956, 966, 993, 1024 (+4 more ranges) |
| 48 | 36 | 84.8% | `crates/app/src/daily_universe_boot.rs` | 215–216, 230, 241, 244, 262, 272, 276, 278–291, 293, 296–299, 317–327 |
| 49 | 34 | 84.0% | `crates/core/src/websocket/activity_watchdog.rs` | 224–225, 236, 239, 241–242, 245, 247, 249–250, 258–260, 263–265, 340, 342–345, 348–353, 360, 362, 364, 368–370, 559 |
| 50 | 33 | 68.9% | `crates/storage/src/boot_probe.rs` | 71, 81, 83, 95–98, 101, 103–106, 113, 121–124, 127, 129–131, 134, 136–138, 141, 143–145, 147, 150–151, 153 |
| 51 | 32 | 81.9% | `crates/storage/src/questdb_health.rs` | 156–158, 165–176, 180–190, 196–200, 206, 244, 254, 261, 269, 284, 290, 340, 347, 358 |
| 52 | 32 | 94.7% | `crates/app/src/apply_reconcile_plan.rs` | 297–305, 457–458, 491–493, 495–496, 517, 531–532, 534, 537, 542–549, 551, 553–555, 557, 561 |
| 53 | 30 | 91.3% | `crates/trading/src/candles/multi_tf_aggregator.rs` | 474, 491, 516, 526, 531, 545, 561, 586, 613, 621, 652 |
| 54 | 30 | 88.0% | `crates/core/src/auth/mid_session_watchdog.rs` | 78–83, 87–92, 96–97, 99–100, 105–110, 255, 261–268 |
| 55 | 29 | 76.4% | `crates/core/src/instrument/csv_downloader.rs` | 115, 128–129, 131–135, 137, 139–140, 162–163, 182, 185–189, 191–195, 198–199, 257, 267 |
| 56 | 29 | 93.9% | `crates/core/src/auth/token_cache.rs` | 95–97, 105, 107–108, 113, 119, 122–123, 127–130, 134–137, 158–161, 214, 224, 310, 320, 369, 423, 427, 437, 441–442, 596–597, 605–606 |
| 57 | 28 | 91.0% | `crates/storage/src/option_chain_minute_snapshot_persistence.rs` | 119–120, 124–127, 129, 161–165, 167–168, 173–174, 176, 180–181, 188, 311–312, 317–322 |
| 58 | 25 | 79.5% | `crates/storage/src/tick_spill_drain.rs` | 106–111, 114–116, 122–123, 128, 130, 138–139, 144, 148, 152–153, 171, 175–176, 178–179, 182–183, 245, 247, 273, 300 |
| 59 | 23 | 80.3% | `crates/core/src/pipeline/no_tick_watchdog.rs` | 99–104, 108–110, 113–114, 116–118, 122–124, 186, 194–199, 242, 261, 281 |
| 60 | 21 | 98.4% | `crates/trading/src/strategy/evaluator.rs` | 476, 490, 592, 638, 708, 731, 771, 777, 816, 852, 911, 917, 952, 1003, 1015, 1074, 1114, 1149, 1181, 1226, 1261, 1299, 1334, 1352, 1414 (+15 more ranges) |
| 61 | 21 | 92.4% | `crates/app/src/subsystem_memory.rs` | 148–149, 171, 221–227, 260, 280–282, 305–307, 309–312 |
| 62 | 20 | 95.8% | `crates/trading/src/oms/reconciliation.rs` | 74–78, 80–81, 104, 121, 131–134, 674–678, 680–681 |
| 63 | 19 | 96.9% | `crates/trading/src/strategy/hot_reload.rs` | 82–83, 102–103, 106, 109–110, 113–114, 156–157, 174, 923, 1010–1014, 1016–1017 |
| 64 | 19 | 93.2% | `crates/storage/src/seal_writer_runner.rs` | 168–182, 256–257, 261, 441 |
| 65 | 18 | 95.2% | `crates/api/src/handlers/debug.rs` | 61, 125, 151, 153, 156, 168, 181, 184–186, 189–197, 199 |
| 66 | 17 | 98.2% | `crates/trading/src/risk/tick_gap_tracker.rs` | 219–221, 254, 319, 428, 519–522, 524, 610, 626, 642, 844, 861, 936, 958, 1050, 1053, 1067–1068, 1146, 1169, 1182, 1606, 1624, 1700, 1759, 1783 |
| 67 | 17 | 81.9% | `crates/trading/src/in_mem/consumer.rs` | 63, 67–75, 77–81, 83–84, 87, 92 |
| 68 | 17 | 91.9% | `crates/core/src/test_support.rs` | 62, 81, 84, 88, 93–95, 123–125, 128–131, 165–167, 170, 172, 211–213, 216, 218, 257–259, 262–265, 303–305, 308–311, 340, 345, 372 |
| 69 | 17 | 85.3% | `crates/core/src/pipeline/prev_close_writer.rs` | 86–87, 90, 93–94, 96–97, 100–101, 104–105, 107, 109–110, 130–131, 135, 163–164, 257, 291 |
| 70 | 17 | 95.7% | `crates/core/src/notification/coalescer.rs` | 127, 248–250, 278, 283, 312, 336, 348, 397–398, 407–408, 417–418, 706, 710 |
| 71 | 16 | 91.4% | `crates/app/src/lifecycle_cache_loader.rs` | 197, 199, 205–220 |
| 72 | 14 | 95.0% | `crates/core/src/instrument/slo_score.rs` | 120, 138–142, 144, 289, 305, 321, 335, 345, 360, 383, 408 |
| 73 | 13 | 95.6% | `crates/storage/src/shadow_candle_writer.rs` | 96–98, 195, 197, 199, 201, 203, 205, 207, 209, 211, 213, 215, 218, 221, 223, 225, 241, 246–252, 256–257 |
| 74 | 13 | 96.8% | `crates/core/src/instrument/instrument_snapshot.rs` | 144, 235–238, 240, 243, 245, 247–248, 265, 275–277, 295, 307, 316 |
| 75 | 13 | 99.0% | `crates/common/src/config.rs` | 160–168, 1406, 1422, 1442–1443, 1448, 1453, 1460, 2670 |
| 76 | 12 | 94.3% | `crates/core/src/instrument/market_open_self_test.rs` | 113, 187–189, 286, 298, 315, 341, 353, 366, 384, 404, 421, 447 |
| 77 | 11 | 96.6% | `crates/storage/src/seal_writer_task.rs` | 138, 146–147, 152–153, 166, 173–175, 181, 183, 210 |
| 78 | 11 | 97.7% | `crates/storage/src/seal_dlq.rs` | 193, 246, 249, 252, 255, 258, 278, 284–285, 291, 312, 325, 752 |
| 79 | 10 | 96.4% | `crates/trading/src/candles/tf_index.rs` | 333–334, 401, 415, 420, 425, 490, 496, 501, 515 |
| 80 | 10 | 95.2% | `crates/core/src/websocket/tls.rs` | 59–61, 95, 107–110, 217, 282, 294 |
| 81 | 10 | 91.5% | `crates/core/src/pipeline/first_seen_set.rs` | 53–55, 162, 174, 176–178, 182–183, 320 |
| 82 | 9 | 99.1% | `crates/trading/src/strategy/config.rs` | 468, 518, 618, 645, 681, 855, 999, 1033, 1064, 1095, 1250 |
| 83 | 9 | 98.7% | `crates/trading/src/risk/engine.rs` | 94, 97–98, 216, 347–351, 353, 401 |
| 84 | 9 | 98.5% | `crates/storage/src/seal_spill.rs` | 337, 390, 395, 398, 418, 428, 432, 442, 455, 479, 949 |
| 85 | 9 | 96.7% | `crates/core/src/instrument/daily_universe_orchestrator.rs` | 139, 158, 162, 164, 180–181, 236–237, 242, 580 |
| 86 | 9 | 98.1% | `crates/api/src/state.rs` | 153–155, 181–183, 193–195 |
| 87 | 8 | 95.7% | `crates/core/src/parser/header.rs` | 106, 116, 204, 235, 245, 255, 265, 275 |
| 88 | 8 | 94.2% | `crates/common/src/market_hours.rs` | 119–121, 123–124, 212, 274, 283–284, 345 |
| 89 | 7 | 97.7% | `crates/core/src/parser/types.rs` | 168, 188, 209, 227, 238, 306, 476 |
| 90 | 7 | 98.2% | `crates/core/src/parser/dispatcher.rs` | 165, 202, 208, 218, 234, 243, 249, 255, 354, 372, 595 |
| 91 | 7 | 96.9% | `crates/core/src/instrument/nse_holiday_cross_check.rs` | 157–158, 289, 293, 297, 305, 311, 318, 324, 352, 367, 384, 414 |
| 92 | 7 | 95.1% | `crates/core/src/instrument/instr_fetch_runner.rs` | 99, 226, 278–282 |
| 93 | 7 | 93.1% | `crates/core/src/instrument/boot_complete_by_guard.rs` | 122, 134, 146, 157, 169, 180, 195 |
| 94 | 7 | 99.1% | `crates/core/src/auth/types.rs` | 32, 51, 146–151, 1309 |
| 95 | 6 | 98.5% | `crates/trading/src/oms/circuit_breaker.rs` | 148–151, 407, 578, 640–641 |
| 96 | 6 | 98.7% | `crates/trading/src/candles/aggregator_cell.rs` | 293–296, 687, 772, 820, 833, 851, 889, 1017 |
| 97 | 6 | 98.3% | `crates/core/src/pipeline/tick_gap_detector.rs` | 38–42, 263 |
| 98 | 6 | 96.8% | `crates/core/src/instrument/index_constituency/parser.rs` | 116, 118, 146–148, 314, 321 |
| 99 | 6 | 94.3% | `crates/core/src/auth/totp_generator.rs` | 59–60, 107, 218–219 |
| 100 | 6 | 98.7% | `crates/common/src/sanitize.rs` | 94–96, 208–209, 807, 853 |
| 101 | 6 | 98.6% | `crates/common/src/error_code.rs` | 1171, 1179–1180, 1199, 1204, 1234 |
| 102 | 6 | 99.2% | `crates/api/src/middleware.rs` | 178–181, 291, 1374 |
| 103 | 5 | 96.9% | `crates/trading/src/orphan_position_watchdog.rs` | 245, 265, 289, 305, 380 |
| 104 | 5 | 90.4% | `crates/trading/src/in_mem/reset_scheduler.rs` | 73–74, 77, 104, 162 |
| 105 | 5 | 95.1% | `crates/core/src/pipeline/volume_monotonicity_guard.rs` | 64–66, 114, 119 |
| 106 | 5 | 96.5% | `crates/core/src/instrument/l3_anomaly_check.rs` | 174, 187, 195, 204, 211, 227, 239, 256 |
| 107 | 5 | 98.9% | `crates/core/src/instrument/daily_universe.rs` | 195, 200, 204, 209, 503, 519, 564, 645 |
| 108 | 5 | 99.1% | `crates/api/src/handlers/quote.rs` | 67–71 |
| 109 | 4 | 98.2% | `crates/trading/src/in_mem/tick_storage.rs` | 143–144, 148–149, 176, 237 |
| 110 | 4 | 98.6% | `crates/core/src/instrument/instr_fetch_loop.rs` | 404 |
| 111 | 4 | 98.6% | `crates/common/src/option_chain_schedule.rs` | 341, 349, 381, 418, 438, 449 |
| 112 | 3 | 95.2% | `crates/trading/src/oms/dh904_backoff.rs` | 121, 138–139 |
| 113 | 3 | 98.7% | `crates/trading/src/in_mem/day_ohlc_tracker.rs` | 239–241 |
| 114 | 3 | 98.4% | `crates/storage/src/seal_writer_loop.rs` | 93–94, 122 |
| 115 | 3 | 98.8% | `crates/core/src/websocket/pool_watchdog.rs` | 202–204, 281, 328, 333, 378, 385, 400, 412, 437, 461, 499 |
| 116 | 3 | 99.3% | `crates/core/src/parser/quote.rs` | 178, 193, 275 |
| 117 | 3 | 97.6% | `crates/core/src/parser/previous_close.rs` | 116, 135, 191 |
| 118 | 3 | 96.8% | `crates/core/src/parser/oi.rs` | 87, 106, 133 |
| 119 | 3 | 92.9% | `crates/core/src/parser/market_status.rs` | 54, 67, 79 |
| 120 | 3 | 99.7% | `crates/common/src/trading_calendar.rs` | 201, 212, 285–286, 296, 300, 302–303, 305–306 |
| 121 | 3 | 99.7% | `crates/common/src/instrument_registry.rs` | 128, 265–266 |
| 122 | 3 | 99.1% | `crates/api/src/handlers/health.rs` | 99, 120, 125 |
| 123 | 2 | 98.8% | `crates/trading/src/risk/types.rs` | 172, 185 |
| 124 | 2 | 99.0% | `crates/trading/src/candles/seal_ring.rs` | 217–218 |
| 125 | 2 | 99.5% | `crates/storage/src/seal_absorption.rs` | 417, 708 |
| 126 | 2 | 95.7% | `crates/core/src/websocket/market_hours_gate.rs` | 93, 99 |
| 127 | 2 | 98.6% | `crates/core/src/parser/ticker.rs` | 132, 198 |
| 128 | 2 | 99.7% | `crates/core/src/parser/full_packet.rs` | 263, 282 |
| 129 | 2 | 98.4% | `crates/core/src/parser/disconnect.rs` | 115, 134 |
| 130 | 2 | 98.1% | `crates/core/src/option_chain/l2_verify.rs` | 189, 198, 213 |
| 131 | 2 | 99.3% | `crates/core/src/instrument/index_extractor.rs` | 237, 340, 360, 371, 462 |
| 132 | 2 | 97.9% | `crates/core/src/instrument/index_constituency/cache.rs` | 82, 84–86, 88–89, 103, 105, 107 |
| 133 | 2 | 99.5% | `crates/core/src/instrument/csv_parser.rs` | 177, 236, 248, 250, 322–323, 325–326, 336–340, 490, 524, 586, 600, 637, 715 |
| 134 | 1 | 99.7% | `crates/trading/src/indicator/obi.rs` | 271 |
| 135 | 1 | 99.9% | `crates/trading/src/indicator/engine.rs` | 546, 548 |
| 136 | 1 | 99.5% | `crates/trading/src/candles/heartbeat.rs` | 382 |
| 137 | 1 | 99.2% | `crates/core/src/pipeline/prev_day_close_stamper.rs` | 91 |
| 138 | 1 | 99.3% | `crates/core/src/option_chain/prev_oi.rs` | 138 |
| 139 | 1 | 99.4% | `crates/core/src/instrument/subscription_distribution.rs` | 150, 205 |
| 140 | 1 | 99.3% | `crates/core/src/instrument/instr_fetch_retry_adapter.rs` | 189 |
| 141 | 1 | 99.8% | `crates/core/src/instrument/fno_underlying_extractor.rs` | 455, 499, 597, 711 |
| 142 | 1 | 99.4% | `crates/core/src/instrument/boot_day_classifier.rs` | 277, 296, 303 |
| 143 | 1 | 99.0% | `crates/app/src/lifecycle_apply.rs` | 69 |

