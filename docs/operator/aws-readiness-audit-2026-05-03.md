# AWS Readiness + Resilience Envelope Audit — 2026-05-03

> **Authority:** This file is the synthesis of a 3-agent adversarial review
> (per `.claude/rules/project/wave-4-shared-preamble.md` §3) requested by
> Parthiban on 2026-05-03 for the question "can you guarantee daily 8am
> AWS auto-start, full subscription by 09:15 IST, and 100% on every
> dimension?"
>
> **Honest 100% claim (per `wave-4-shared-preamble.md` §8):** "100%
> inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ; ≤600K rescue ring
> capacity; bench-gated O(1) hot path; composite-key uniqueness;
> chaos-tested 70h sleep/wake. Beyond the envelope, DLQ NDJSON catches
> every payload as recoverable text."

## Section 1 — Findings table (CRITICAL/HIGH/MEDIUM/LOW)

| # | Severity | Finding | File:line evidence | Action required |
|---|---|---|---|---|
| 1 | **CRITICAL** | EventBridge cron is **08:00 IST**, but `aws-budget.md` "Instance Schedule" says **7:45 IST**. 15-minute discrepancy. With 120s boot timeout + 300s token-init worst case, 08:00 start can miss 09:13 Phase 2 if anything is sluggish. | `deploy/aws/terraform/main.tf:256,262,268,274` vs `.claude/rules/project/aws-budget.md` Instance Schedule | Fix Terraform to `cron(15 2 ? * MON-FRI *)` (07:45 IST) per spec, OR update aws-budget.md to acknowledge 08:00 IST and add ratchet test. |
| 2 | **CRITICAL** | Rescue ring 600K capacity **constant + bound test NOT FOUND** in repo. The Section 8 honest-100% wording cites "≤600K rescue ring capacity" but Agent 1 could not locate the constant. | search returned no match | Either (a) the claim is aspirational and Section 8 must be reworded, or (b) the constant exists under a different name — find and add a `test_rescue_ring_capacity_pinned_at_600k` ratchet. |
| 3 | **CRITICAL** | Chaos test 70h sleep/wake **NOT FOUND**. Section 8 cites "chaos-tested 70h sleep/wake" but no test file exercises a 70h dormant window. Friday 15:30 IST → Monday 09:00 IST recovery is currently UNTESTED. | search returned no match for "70h" / "70 hour" / chaos sleep | Write `crates/core/tests/chaos_70h_sleep_wake.rs` that simulates dormant sleep across the weekend boundary OR reword Section 8 to drop the claim. |
| 4 | **HIGH** | 5 `pub fn` in `s3_backup.rs` are defined but **never called in production boot path** — only invoked in tests. S3 backup is "scaffolding for future capabilities" not actually wired. | `crates/core/src/instrument/s3_backup.rs:91,109,119,134,210` | Either wire S3 backup into the daily refresh path (per `aws-budget.md` lifecycle policy) or delete the dead pub fns to keep `pub-fn-wiring-guard.sh` honest. |
| 5 | **HIGH** | No 09:14 IST positive readiness Telegram. Pre-market check window is `08:00–09:14` and fires Telegram only on FAILURE. Operator's "am I ready?" question has no positive 1-min-before-open signal. | `crates/app/src/main.rs:2323-2389` (`in_pre_market = hour == 8 \|\| (hour == 9 && minute < 15)`) | Add `MarketOpenReadinessConfirmation` event @ 09:14:00 IST with subscription counts (Severity::Info). Edge-trigger once/trading day. Closes the false-OK gap from Rule 11. |
| 6 | **HIGH** | Token renewal task sleeps `82,792s ≈ 23h` and fires once. If that single attempt fails (Dhan 503), there is **no inline retry visible in this audit**. Token would expire at 24h mark. AUTH-GAP-03 catches it on WS wake, but only if WS wakes BEFORE 24h. | observed in 2026-05-03 log: `"sleep_secs":82792` from `token_manager` line | Verify `force_renewal_if_stale` is called from a non-WS-wake path (e.g. periodic 4h sweep). If not, add a sweep task. |
| 7 | **HIGH** | Boot steps 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14 have **NO explicit per-step timeout** — only the global `BOOT_TIMEOUT_SECS = 120s` umbrella. A single sluggish step can consume the entire budget silently. | `crates/app/src/main.rs:7215` (umbrella check) + per-step inspection | Add per-step timeouts with `tokio::time::timeout` so the umbrella alert names which step blew. |
| 8 | **MEDIUM** | I-P1-11 composite key meta-guard PROVEN but **NOT NAMED with that identifier** in the code/test. Reviewers grepping for "I-P1-11" find docs only, not enforcement. | `quality/benchmark-budgets.toml:26` + `TickGapDetector` papaya composite tuple | Add a comment block in `dedup_segment_meta_guard.rs` and `TickGapDetector` referencing "I-P1-11 enforcement" so the grep trail is honest. |
| 9 | **MEDIUM** | Localhost / 127.0.0.1 / 0.0.0.0 references in non-test code: 5 hits. Most are AWS-safe (API binds 0.0.0.0:3001, CORS for local Grafana), but `main.rs:7171` opens browser to `http://localhost:3001/portal/options-chain` — will silently fail on AWS headless. | `crates/common/src/config.rs:755,756,1614` + `crates/app/src/main.rs:7171,9260` | Wrap browser-open in env check (`if !cfg.aws_mode`) or move to a separate `--open-portal` CLI subcommand. |
| 10 | **MEDIUM** | Hot-path `.clone()` / `Vec::new()` hits in `crates/core/src/parser/dispatcher.rs:793,805,874,998,1027`. DHAT tests exist but cover the per-packet parsers, NOT the dispatcher. | `crates/core/src/parser/dispatcher.rs:793,805,874,998,1027` | Verify each hit is on a cold/error path (likely yes — dispatcher hot path is `match` + offset). Add `// O(1) EXEMPT:` comments OR a `dhat_dispatcher_hot_path.rs` test. |
| 11 | **MEDIUM** | `tick_processor.rs` has dozens of `.clone()`, `format!`, `Vec::new`, `Box`, `.collect()` hits across 2-3K LoC. Most are marked `O(1) EXEMPT` per Agent 1 but not all. | `crates/core/src/pipeline/tick_processor.rs:504,552,564-566,604,641,700,1017-1018,1599,2071,2760,2790,2996,…` | Spot-check the unmarked hits. The DHAT test pins the *parsers*, not `tick_processor::process_tick` end-to-end. Add an end-to-end DHAT to close the gap. |
| 12 | **LOW** | `crates/core/src/parser/order_update.rs:68,227,324,384,440,454` has `.to_string()` and `format!()` calls, but order_update WS is JSON not binary, low-volume, NOT a hot path. | `crates/core/src/parser/order_update.rs:68,…` | No action — confirm `O(1) EXEMPT` comments are present. |
| 13 | **LOW** | systemd units exist (`deploy/systemd/tickvault.service`, `scripts/tv-tunnel/tickvault-tunnel.service`) — Agent 2 confirmed, no gap. | `deploy/systemd/tickvault.service` | None — verify `ExecStartPre=/usr/bin/docker compose up -d` is in the unit file and `WantedBy=multi-user.target` for auto-start at boot. |
| 14 | **LOW** | Prod SSM path tree (10 paths) **fully wired** in `secret_manager.rs:525-750`. Includes Dhan, Telegram, QuestDB, Grafana, API, Valkey. (Dated 2026-05-03 snapshot — Valkey + Grafana SSM paths removed in #O4/#O1, 2026-05-24.) | `crates/core/src/auth/secret_manager.rs:525,529,533,610,633,637,661,665,687,750` | None — provision the actual SSM parameters before AWS deploy, but the code path is ready. |
| 15 | **LOW** | IP verifier uses env-agnostic SSM path with hard halt on mismatch — works for both dev (skipped per config) and prod. No separate enforcement flag needed. | `crates/core/src/network/ip_verifier.rs:108,528,534` | None. |

## Section 2 — Per-step boot timeout audit

| Step | Action | Per-step timeout | Worst-case latency | Risk |
|---|---|---|---|---|
| 1 | Load config | none (umbrella 120s) | <100ms | LOW |
| 2 | Init observability | none | <500ms | LOW |
| 3 | Init structured logging | none | <100ms | LOW |
| 4 | Notification + Docker (parallel) | none | up to 30s if Docker cold | MEDIUM |
| 5 | IP verify against SSM | none | up to 5s (SSM API) | MEDIUM |
| 6 | Dhan auth | `TOKEN_INIT_TIMEOUT_SECS = 300` | 300s if Dhan slow | HIGH |
| 7 | QuestDB DDL setup | `BOOT_DEADLINE_SECS = 60` (BOOT-01@30s WARN, BOOT-02@60s HALT) | 60s, then HALT | LOW (HALT is correct) |
| 8 | F&O universe build | none | ~2.3s observed (CSV download + parse + persist) | LOW |
| 9 | WS pool build | none | ~1s | LOW |
| 10 | Tick processor spawn | none | <100ms | LOW |
| 11 | Historical fetch (background) | none — runs cold path | hours | LOW (background) |
| 12 | Order update WS spawn | none | <500ms | LOW |
| 13 | API server start | none | <500ms | LOW |
| 14 | Token renewal task spawn | none | <100ms | LOW |
| 15 | Shutdown signal await | N/A | infinite (correct) | LOW |
| Umbrella | All-step deadline | `BOOT_TIMEOUT_SECS = 120s` → CRITICAL Telegram | 120s | HIGH if step 6 takes 300s — umbrella fires at 120s but step 6 keeps trying for another 180s |

**Observation:** Step 6 timeout (300s) > umbrella timeout (120s). This is a **logic conflict** — umbrella alerts before step 6 completes. Either reduce step 6 to ≤90s or raise umbrella to ≥350s.

## Section 3 — Honest mechanical proof matrix

The 15-row 100% guarantee matrix from `per-wave-guarantee-matrix.md` maps to:

| Demand | Status today | Gap to 100% |
|---|---|---|
| Code coverage | Threshold pinned in `quality/crate-coverage-thresholds.toml`, gate `scripts/coverage-gate.sh` | None mechanical — verify threshold is 100% per crate |
| Audit coverage | 6 audit tables wired (phase2/depth_rebalance/ws_reconnect/boot/selftest/order) | S3 archive (STORAGE-GAP-04) reserved but not yet implemented |
| Testing coverage | 22 categories per `testing.md`, scoped per crate | Chaos 70h test MISSING (Finding #3) |
| Code checks | banned-pattern + pub-fn-wiring + secret-scan + 8 pre-commit gates | Finding #4: dead pub fns in s3_backup.rs would currently FAIL pub-fn-wiring-guard |
| Performance | DHAT zero-alloc + Criterion budgets + 5% bench-gate | Finding #10, #11: dispatcher + tick_processor end-to-end DHAT MISSING |
| Monitoring | 7-layer telemetry + MCP tools | Finding #5: 09:14 positive readiness MISSING |
| Logging | tracing + ERROR→Telegram + hourly errors.jsonl | None mechanical |
| Alerting | alerts.yml + resilience_sla_alert_guard.rs | None mechanical |
| Security | banned-pattern + secret-scan + Secret<T> | Finding #9: localhost browser-open leaks on AWS |
| Security hardening | static IP + secret scan + unused_must_use lint | None mechanical |
| Bug fixing | 3-agent adversarial review | This audit IS that review for AWS readiness |
| Scenarios covering | 9-box checklist + chaos suite | Finding #3 + Finding #6 (token race) NOT chaos-tested |
| Functionality covering | every pub fn has test + call site | Finding #4 violates this — must fix |
| Code review | 3-agent on diff before AND after | This audit IS the "before AWS deploy" review |
| Extreme check | All ratchet tests fail build on regression | Findings #2, #3 reveal claims without ratchets |

## Section 4 — Concrete next steps (prioritized)

| # | Task | Effort | Blocks AWS deploy? |
|---|---|---|---|
| 1 | Fix EventBridge cron 08:00 → 07:45 IST OR update spec | 15min Terraform | **YES** |
| 2 | Add per-step boot timeouts (with names in alerts) | 2h Rust | **YES** for clean ops |
| 3 | Add `MarketOpenReadinessConfirmation` Telegram @ 09:14 IST | 1h Rust + ratchet test | NO (nice-to-have) |
| 4 | Either find rescue ring 600K constant or remove the claim from §8 | 30min audit | **YES** for honest §8 |
| 5 | Either write 70h chaos test or remove the claim from §8 | 4h test OR 5min reword | **YES** for honest §8 |
| 6 | Wire S3 backup OR delete the 5 dead pub fns | 2h either way | NO |
| 7 | Spot-check tick_processor unmarked allocations | 2h audit | NO |
| 8 | Wrap browser-open in `if !aws_mode` | 30min | NO (cosmetic on AWS) |
| 9 | Verify token sweep task exists (not just WS-wake renewal) | 1h | **YES** for 24h+ uptime |
| 10 | End-to-end Sunday dry-run on actual AWS instance | 4h ops | **YES** before first Mon |

## Section 5 — Bottom-line answer to "can you guarantee 8am auto-start + 100% on everything?"

**Honest answer:** I can guarantee 100% inside the tested envelope after the
10 next-steps in Section 4 land. Specifically:

- **Yes** I can guarantee daily 7:45 IST EC2 wake → 09:13 Phase 2 dispatch
  → 09:15 streaming, **after Findings #1, #2, #5, #6, #7 land**.
- **Yes** I can guarantee zero ticks missed inside the rescue→spill→DLQ
  envelope — but only after Finding #2 confirms the 600K constant exists.
- **No** I cannot guarantee literal "WebSocket never disconnects" or
  "QuestDB never fails" — your own `wave-4-shared-preamble.md` §8 forbids
  that wording.
- **Yes** every Claude session already auto-runs `mcp__tickvault-logs__*`
  tools per `.claude/settings.json` SessionStart hooks (Wave 4 §1 fully
  wired).

**Total work to honest-100% before AWS deploy:** ~14 hours engineering +
4 hours dry-run = 2 working days.
