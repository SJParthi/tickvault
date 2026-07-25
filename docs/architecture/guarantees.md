# Guarantees Matrix — every "100%" claim mapped to its mechanical proof

> **Authority:** this document is the definitive answer to the operator
> ask: *"I need 100 percentage guarantee ... without hallucination or
> illusion ... I need proof bro."*
>
> **Rule:** every row below lists ONE claim + the exact file / CI gate
> / test that enforces it. If the proof file goes missing or the
> guard regresses, `make validate-automation` fails and no PR can merge.
>
> **Honesty clause:** physics-impossible claims (e.g. "WebSocket never
> disconnects") are replaced with **measurable SLAs** that fire alerts
> on violation. Both columns are honest about which is which.
>
> **Plain-English version** (no jargon, no file names — for anyone):
> `docs/architecture/what-we-guarantee-plain-english.md`. Same truth, human words.

## How to use this file

If an operator or Claude session asks "how do you guarantee X?" —
find the row, open the proof file, run the test. There is no row
where the answer is "trust me".

## Tier 1 — Observability chain (mechanically provable)

| Claim | Proof file | Test name |
|---|---|---|
| Every `error!` reaches Telegram (Loki→ERROR routing) + CloudWatch alarms→SNS 4-channel fan-out | `crates/core/src/notification/service.rs` + `deploy/aws/terraform/app-alarms.tf` | `crates/common/tests/cloudwatch_app_alarms_wiring.rs` (the Prometheus `tickvault-alerts.yml` + `resilience_sla_alert_guard` cited here previously were retired #O2/#O3) |
| No flush/drain/persist failure is silenced as WARN | `crates/storage/tests/error_level_meta_guard.rs` | `flush_persist_broadcast_failures_must_use_error_level` |
| 54 error codes, every one documented in a rule file | `crates/common/src/error_code.rs` | `every_error_code_variant_appears_in_a_rule_file` |
| Every rule-file code has an ErrorCode variant | `crates/common/tests/error_code_rule_file_crossref.rs` | `every_rule_file_code_has_an_enum_variant` |
| Every ErrorCode runbook path exists on disk | same file | `every_runbook_path_exists_on_disk` |
| `error!` messages that mention a code carry `code =` field | `crates/common/tests/error_code_tag_guard.rs` | `every_error_macro_tagged_with_a_known_code_carries_code_field` |
| Critical codes NEVER auto-triage | `crates/common/src/error_code.rs` | `test_critical_codes_never_auto_triage` |
| Every triage-rule auto-fix script exists + is executable | `crates/common/tests/triage_rules_guard.rs` | `every_auto_fix_script_exists_and_is_executable` |
| Every triage-rule code matches an ErrorCode | same | `every_code_in_rules_yaml_matches_an_error_code_variant` |
| Every triage-rule confidence ∈ [0.0, 1.0] | same | `every_confidence_value_is_in_range` |

## Tier 2 — Zero-tick-loss + resilience SLAs

Physics limit: we cannot force Dhan's servers to stay connected.
But we CAN mechanically guarantee:

> **2026-06-02 accuracy fix:** the prior rows here cited Prometheus rule
> file `tickvault-alerts.yml` + `resilience_sla_alert_guard` — both were
> DELETED in the CloudWatch-only migration (#O2/#O3). The real proof is now
> the CloudWatch alarm stack in `deploy/aws/terraform/app-alarms.tf`,
> ratcheted by `crates/common/tests/cloudwatch_app_alarms_wiring.rs`
> (3-way EMF↔alarm↔Rust-emit wiring check).

| Claim | Proof file | Test name |
|---|---|---|
| CloudWatch alarm fires when `tv_ticks_dropped_total > 0` (the FINAL irrecoverable tick-loss breach — added #989) *(RETIRED 2026-07-18, stage-4 dead-producer sweep: the alarm + its metric's emit sites are deleted — the tick ring/spill/DLQ chain died with the stage-2 dead-WS sweep 2026-07-17; the seal-side pagers in seal-drop-alarm.tf remain the loss monitors.)* | `deploy/aws/terraform/app-alarms.tf` (`tv-${env}-ticks-dropped`) | `crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_app_alarms_count_is_thirteen` |
| Alarm fires when disk spill is dropping (`tv_spill_dropped_total`) *(RETIRED 2026-07-18, stage-4 dead-producer sweep: the alarm + its metric's emit sites are deleted — the tick ring/spill/DLQ chain died with the stage-2 dead-WS sweep 2026-07-17; the seal-side pagers in seal-drop-alarm.tf remain the loss monitors.)* | same tf (`tv-${env}-spill-dropped`) | same guard (every alarm metric has a Rust emit-site) |
| Alarm fires when DLQ catches ticks (`tv_dlq_ticks_total`) *(RETIRED 2026-07-18, stage-4 dead-producer sweep: the alarm + its metric's emit sites are deleted — the tick ring/spill/DLQ chain died with the stage-2 dead-WS sweep 2026-07-17; the seal-side pagers in seal-drop-alarm.tf remain the loss monitors.)* | same tf (`tv-${env}-dlq-ticks`) | same guard |
| Every alarm metric is published by the CW agent (EMF filter) | `deploy/aws/terraform/user-data.sh.tftpl` | `cloudwatch_app_alarms_wiring.rs::test_every_alarm_metric_is_in_emf_filter_list` |
| Every alarm metric has a real Rust emit-site (no dead alarms) | crates/ (`counter!`/`gauge!`) | `cloudwatch_app_alarms_wiring.rs::test_every_alarm_metric_has_a_rust_emit_site` |
| `ticks_dropped_total` counter emission *(RETIRED 2026-07-18, stage-4 sweep: `tick_persistence.rs` was deleted in the stage-2 dead-WS sweep — zero emit sites remain; `zero_tick_loss_alert_guard.rs` deleted with it)* | — | — |
| `SEAL_BUFFER_CAPACITY = 200,000` (absorbs the IST-midnight seal burst) | `crates/trading/src/candles/seal_ring.rs` | `seal_ring.rs::test_seal_buffer_capacity_constant_is_locked_value` |
| QuestDB outage drops zero ticks (disconnected writer) | `crates/storage/tests/chaos_questdb_full_session.rs` | `chaos_questdb_full_session_zero_tick_loss` (run 2026-06-02: 0 lost) |
| Disk-full → DLQ NDJSON catches every tick | `crates/storage/tests/chaos_disk_full.rs` | `chaos_disk_full_triggers_dlq` (`#[ignore]` — run 2026-06-02: 0 lost) |
| SIGKILL mid-batch → spill replay loses zero ticks | `crates/storage/tests/chaos_sigkill_replay.rs` | `chaos_sigkill_spill_replay_zero_loss` (`#[ignore]` — run 2026-06-02: 0 lost) |
| Spill ring saturation (50 churn cycles) — no leak, no panic | `crates/storage/tests/chaos_ws_frame_spill_saturation.rs` | `chaos_rapid_spill_churn_50_cycles_no_leak_no_panic` |
| WAL is fail-closed at boot (no silent-loss degraded mode) | `crates/app/src/main.rs` | `crates/core/tests/phase2_7_perf_and_correctness_fixes.rs::test_regression_ws_frame_wal_init_is_fail_closed` |

## Tier 3 — O(1) hot-path + zero-allocation

| Claim | Proof file | Test name |
|---|---|---|
| Tick parse ≤ 10 ns (p99) | `quality/benchmark-budgets.toml` + Criterion benches | bench gate via `scripts/bench-gate.sh` |
| Pipeline routing ≤ 100 ns per tick (gate now divides batch median by element count — honest per-tick, fixed #992; measured 2026-06-02: 4.7 ns/tick) | same + `quality/benchmark-budgets.toml` `[elements]` | `crates/common/tests/bench_budget_elements_guard.rs` |
| papaya registry lookup ≤ 50 ns (measured 2026-06-02: 10.6 ns, hit≈miss → true O(1)) | same | bench gate |
| OMS state transition ≤ 100 ns | same | same |
| Zero heap alloc per tick | `crates/core/tests/dhat_allocation.rs` | DHAT harness, fails CI on non-zero alloc |
| 5% benchmark regression = build fail | `quality/benchmark-budgets.toml` | bench-gate CI stage |
| No `.unwrap()` / `.expect()` in prod | every `crates/*/src/lib.rs` | `#![cfg_attr(not(test), deny(clippy::unwrap_used))]` |
| No dropped `Result` in prod | every `crates/*/src/lib.rs` | `#![cfg_attr(not(test), deny(unused_must_use))]` |
| No `.clone()` on hot path | `.claude/hooks/banned-pattern-scanner.sh` category 2 | pre-commit hook |
| No `DashMap` (must use papaya) | same | pre-commit hook |

## Tier 4 — Uniqueness + deduplication

| Claim | Proof file | Test name |
|---|---|---|
| `security_id` alone is NOT used as a key | `.claude/hooks/banned-pattern-scanner.sh` category 5 | pre-commit hook |
| Every storage DEDUP key includes segment | `crates/storage/tests/dedup_segment_meta_guard.rs` | meta-guard scans every `DEDUP_KEY_*` constant |
| `InstrumentRegistry` keeps BOTH colliding entries | `crates/common/src/instrument_registry.rs` | `test_iter_returns_both_colliding_segments` |
| Dhan CSV dedup uses `(id, segment)` tuple | `crates/core/src/instrument/universe_builder.rs` | compile-time type guard |
| Subscription planner dedup uses tuple | `crates/core/src/instrument/subscription_planner.rs` | `test_regression_seen_ids_key_type_is_pair` |
| Tick dedup key includes segment | `crates/storage/src/tick_persistence.rs` | `test_tick_dedup_key_includes_segment` |
| Sequence-hole detector exists | `crates/trading/src/risk/tick_gap_tracker.rs` | 48 unit tests |
| Backwards-timestamp-jump detector exists | same (via PR #277) | `test_backwards_jump_*` (6 tests) |

## Tier 5 — Test coverage + code quality

| Claim | Proof file | Test name / gate |
|---|---|---|
| Ratcheted per-crate LINE-coverage floors (app 68.3 · core 91.6 · storage 90.1 · trading 96.9 · api 98.6 · common 99.5 · default 63.0; floors only move up, 100% is the target — LINE coverage only, no branch gate) | `quality/crate-coverage-thresholds.toml` | `scripts/coverage-gate.sh`, post-merge only (not a PR blocker) |
| Mutation score: zero survivors | `.github/workflows/mutation.yml` | lines 103-113 fail if any `SURVIVED` |
| Fuzz targets run weekly, zero crashes | `.github/workflows/fuzz.yml` | nightly schedule |
| `cargo fmt --check` clean | CI | pre-commit + PR gate |
| `cargo clippy -- -D warnings` clean | CI | pre-commit + PR gate |
| `cargo audit` zero CVEs | CI security job | nightly + PR gate |
| 22 test categories scoped by crate | `scripts/test-coverage-guard.sh` | `.claude/hooks/pre-push-gate.sh` gate 11 |

## Tier 6 — Security + hardening

| Claim | Proof file | Test name |
|---|---|---|
| No secrets in code | `.claude/hooks/secret-scanner.sh` | pre-commit hook |
| `.env` files never committed | same | pre-commit hook |
| All secrets via AWS SSM | `crates/core/src/auth/secret_manager.rs` | runtime check at boot |
| Secret types zeroized on drop | `secrecy = 0.10.3` dep | compile-time |
| Cargo deps pinned exact (no `^`/`~`/`*`/`>=`) | `Cargo.toml` + `deny.toml` | `cargo deny check` |
| `cargo update` is banned | `.claude/rules/project/cargo-and-docker.md` | rule enforcement |
| No `localhost` hardcoded | `.claude/hooks/banned-pattern-scanner.sh` | pre-commit hook |
| No `:latest` Docker tags | `deploy/docker/docker-compose.yml` | SHA256 digests pinned |
| Static IP enforcement for orders | `crates/core/src/network/ip_monitor.rs` | `GAP-NET-01` guard |
| Dry-run mode defaults true | `crates/trading/src/oms/engine.rs` | `OMS-GAP-06` test |
| Auth circuit breaker 3-state FSM | `crates/trading/src/oms/circuit_breaker.rs` | `OMS-GAP-03` tests |
| TOTP + 24h JWT cycle | `crates/core/src/auth/token_manager.rs` | `AUTH-GAP-01` tests |

## Tier 7 — Real-time monitoring + viewability

| Claim | Proof file | Mechanism |
|---|---|---|
| Every ERROR in JSONL within ~1s | `crates/app/src/observability.rs` | `init_errors_jsonl_appender` (tracing-appender 0.2.3) |
| 48h retention auto-swept | same | `sweep_errors_jsonl_retention` tokio task |
| Summary markdown regenerates every 60s | `crates/core/src/notification/summary_writer.rs` | tokio interval |
| Operator dashboards (CloudWatch) | AWS CloudWatch Dashboards | provisioned via `deploy/aws/terraform/` _(the 5 local Grafana dashboards + `operator_health_dashboard_guard` were retired in the CloudWatch-only migration #O1, 2026-05-19)_ |
| Operator alarms (CloudWatch) | AWS CloudWatch Alarms over the app metrics | _(the Prometheus recording/alert rules `tickvault-alerts.yml` + `recording_rules_guard` were retired #O2/#O3)_ |
| ERROR logs → CloudWatch Logs | CloudWatch Logs metric filters / alarms | _(the opt-in Loki LogQL alerts + Loki/Alloy containers were retired in the CloudWatch-only migration)_ |
| `make doctor` = 7-section health in one command | `scripts/doctor.sh` | run any time |
| 12 MCP tools for Claude workspace access | `crates/tickvault-logs-mcp` _(2026-07-18 Rust cutover — the Python server was deleted; launched via `scripts/mcp-servers/tickvault-logs-launch.sh`)_ | `tickvault_logs_mcp_guard` |

## Tier 8 — Automation surface (100% searchable/findable)

Claude Code can answer ANY operator question with ONE tool call —
no shell, no curl, no grep:

| Question | Tool |
|---|---|
| "What errors in the last hour?" | `mcp__tickvault-logs__tail_errors` |
| "What novel patterns?" | `list_novel_signatures` |
| "Current error summary?" | `summary_snapshot` |
| "Triage audit trail?" | `triage_log_tail` |
| "Full history of signature X?" | `signature_history` |
| "What's the current metric X?" | CloudWatch metrics / app `/metrics` exporter (the `prometheus_query` MCP tool was retired in #O5, 2026-05-30) |
| "Which runbook for code Y?" | `find_runbook_for_code` |
| "What's firing right now?" | CloudWatch alarms / `run_doctor` (the `list_active_alerts` MCP tool was retired in #O5, 2026-05-30) |
| "Run SQL query Z" | `questdb_sql` |
| "Find pattern P in the codebase" | `grep_codebase` |
| "Is the system healthy?" | `run_doctor` |
| "What changed recently?" | `git_recent_log` |

## Tier 9 — CI/CD gates (no PR merges if any fail)

| Gate | Blocks merge? | Runs |
|---|---|---|
| `cargo fmt --check` | YES | every push |
| `cargo clippy -- -D warnings` | YES | every push |
| 22-test scope | YES | changed crates |
| Banned-pattern scanner (6 categories) | YES | pre-commit + CI |
| Secret scan | YES | pre-commit + CI |
| `cargo audit` | YES | every push |
| `cargo deny` | YES | every push |
| 30-check validate-automation | YES | every push |
| Ratcheted per-crate LINE-coverage floors (68.3–99.5, target 100%; no branch gate) | YES | post-merge only (not a PR blocker) |
| Mutation zero-survivors | YES | weekly |
| Fuzz 0 crashes | YES | weekly |

## Tier 10 — What's explicitly NOT guaranteed (and why)

Physics / economics limits. These claims are **replaced by SLAs**:

| Impossible claim | SLA replacement | Alert |
|---|---|---|
| "WebSocket never disconnects" | Reconnect p99 < 500ms, zero tick loss via ring + spill | `HighWebSocketReconnectRate` |
| "QuestDB never fails" | Back-pressure + disk spill + auto-resume < 30s | `QuestDbDown` |
| "No future bugs" | 0 known bugs at HEAD + every seen pattern = CI block | every guard above |
| "100% of all possible scenarios" | Every enumerated scenario + fuzz + mutation | weekly fuzz / mutation CI |
| "Auto-resolve every incident" | Known-signature → auto-fix; novel → escalate with context | triage hook + YAML rules |

## Tier 11 — Session 2026-06-02 hardening (new guarantees, evidence-cited)

Added this session; every row points at a real test created alongside it.

| Claim | Proof file | Test name |
|---|---|---|
| Integration tests run on EVERY PR (not just `--lib`) — the #988 silent-rot class is dead | `.github/workflows/ci.yml` (test matrix) | `crates/common/tests/github_workflow_guard.rs::r17_ci_test_matrix_runs_integration_tests_not_just_lib` |
| The 10 rotted integration targets are repaired + green | `crates/core/tests/*` (dhat_allocation, snapshot_parser, ws_*, phase2_*, mutation_killer) + `crates/common/tests/triage_rules_full_coverage_guard.rs` | all green in #988 + #991 CI |
| Final irrecoverable tick-loss has a dedicated CloudWatch alarm | `deploy/aws/terraform/app-alarms.tf` | `cloudwatch_app_alarms_wiring.rs::test_app_alarms_count_is_thirteen` |
| Post-merge auto-deploy is covered by an AWS-native watchdog (GitHub-cron miss safety-net) | `deploy/aws/terraform/deploy-watchdog-lambda.tf` + `deploy/aws/lambda/deploy-watchdog/` | `crates/common/tests/aws_infra_wiring.rs::test_deploy_watchdog_lambda_is_wired` + `..._only_dispatches_when_positively_stale` |
| `change_pct` + `open_gap_pct` candle columns wired end-to-end (DDL + ALTER + ILP + struct + spill) | `crates/storage/src/{shadow_persistence,shadow_candle_writer,shadow_seal_columns,seal_spill}.rs` | `crates/storage/tests/candle_pct_column_guard.rs::{change_pct,open_gap_pct}_column_wired_end_to_end` |
| Latency bench gate enforces PER-TICK budgets (batch-vs-per-tick unit bug fixed) | `scripts/bench-gate.sh` + `quality/benchmark-budgets.toml` `[elements]` | `bench_budget_elements_guard.rs` (2 tests) |

## How to prove this matrix is real (right now)

```bash
# Every row above points at a file. Every file exists:
make validate-automation
# Expected: 30/30 PASS

# Every row's test passes:
cargo test --workspace --no-fail-fast

# Doctor confirms live infra:
make doctor

# MCP tools are live (2026-07-18 Rust cutover — the Python server was
# deleted; the launcher runs crates/tickvault-logs-mcp):
bash scripts/mcp-servers/tickvault-logs-launch.sh --self-test
```

If any of these 4 commands fails, the guarantee is broken — and the
failing test names the exact subsystem to fix.

## How to add a new guarantee

1. Write the guard (test or CI gate).
2. Add a row to this file with a link to the guard.
3. Wire the guard into `scripts/validate-automation.sh`.
4. Open a PR. CI confirms the guard catches the violation.
5. Merge.

**No guarantee that isn't backed by a file in this table exists.**
