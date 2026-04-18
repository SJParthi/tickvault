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

## How to use this file

If an operator or Claude session asks "how do you guarantee X?" —
find the row, open the proof file, run the test. There is no row
where the answer is "trust me".

## Tier 1 — Observability chain (mechanically provable)

| Claim | Proof file | Test name |
|---|---|---|
| Every `error!` reaches Telegram within ~15s | `deploy/docker/prometheus/rules/tickvault-alerts.yml` + `crates/core/src/notification/service.rs` | `resilience_sla_alert_guard` |
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

| Claim | Proof file | Test name |
|---|---|---|
| Prometheus alert fires when `tv_ticks_dropped_total > 0` | `deploy/docker/prometheus/rules/tickvault-alerts.yml` | `prometheus_alert_rule_ticks_dropped_exists` |
| Alert fires when ring buffer active | same yaml | `prometheus_alert_rule_tick_buffer_active_exists` |
| Alert fires when disk spill active | same yaml | `prometheus_alert_rule_tick_disk_spill_active_exists` |
| Catch-all data-loss alert exists | same yaml | `prometheus_alert_rule_tick_data_loss_exists` |
| `ticks_dropped_total` counter field still emitted from storage | `crates/storage/src/tick_persistence.rs` | `tick_persistence_emits_dropped_counter_field` |
| `TICK_BUFFER_CAPACITY >= 100,000` (absorbs market bursts) | `crates/common/src/constants.rs` | `tick_persistence_buffer_capacity_is_at_least_one_lakh` |
| 30s QuestDB outage drops zero ticks | `crates/storage/tests/chaos_zero_tick_loss.rs` | `chaos_questdb_outage_30s_loses_zero_ticks` (`#[ignore]` — run manually) |
| 60s outage triggers spill but zero drops | same | `chaos_prolonged_outage_60s_triggers_disk_spill` (`#[ignore]`) |
| WS/QuestDB/Valkey SLA alerts all wired | `crates/storage/tests/resilience_sla_alert_guard.rs` | `every_resilience_sla_alert_is_pinned` |
| Every SLA alert has a severity label | same | `every_resilience_alert_has_severity_label` |
| QuestDB liveness gauge emitted somewhere in storage | same | `tick_persistence_emits_questdb_connection_gauge` |

## Tier 3 — O(1) hot-path + zero-allocation

| Claim | Proof file | Test name |
|---|---|---|
| Tick parse ≤ 10 ns (p99) | `quality/benchmark-budgets.toml` + Criterion benches | bench gate via `scripts/bench-gate.sh` |
| Pipeline routing ≤ 100 ns per tick | same | same |
| papaya registry lookup ≤ 50 ns | same | same |
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
| 100% line coverage required per crate | `quality/crate-coverage-thresholds.toml` | `scripts/coverage-gate.sh` in CI |
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
| 5 Grafana dashboards auto-provisioned | `deploy/docker/grafana/provisioning/` | Grafana reads on boot |
| Operator dashboard pins 14 panels | `crates/storage/tests/operator_health_dashboard_guard.rs` | 7 structural tests |
| Prometheus recording rules speed up dashboards | `deploy/docker/prometheus/rules/tickvault-alerts.yml` | 10 `tv:*` pre-aggregations |
| Every recording rule pinned | `crates/common/tests/recording_rules_guard.rs` | 7 tests |
| Loki LogQL alerts (opt-in) | `deploy/docker/loki/rules.yml` | 4 alert rules + guard |
| `make doctor` = 7-section health in one command | `scripts/doctor.sh` | run any time |
| 12 MCP tools for Claude workspace access | `scripts/mcp-servers/tickvault-logs/server.py` | `tickvault_logs_mcp_guard` |

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
| "What's the current metric X?" | `prometheus_query` |
| "Which runbook for code Y?" | `find_runbook_for_code` |
| "What's firing right now?" | `list_active_alerts` |
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
| Line + branch coverage 100% | YES | post-merge |
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

## How to prove this matrix is real (right now)

```bash
# Every row above points at a file. Every file exists:
make validate-automation
# Expected: 30/30 PASS

# Every row's test passes:
cargo test --workspace --no-fail-fast

# Doctor confirms live infra:
make doctor

# MCP tools are live:
python3 scripts/mcp-servers/tickvault-logs/server.py --self-test
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
