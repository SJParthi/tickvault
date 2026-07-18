# Implementation Plan: Rust-Only Purge — Phase 2 (rewrite the LIVE Python in Rust)

**Status:** APPROVED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator) — standing full-authority instruction (session preamble) + coordinator rust-only directive 2026-07-18 (verbatim: "our entire tickvault repository should be entirely RUST O(1) even now and in the future always — nowhere should even the word python be used")

> Design source of record: the Phase-2 design doc (evidence base origin/main
> @ `2bf95e7b`, review round 1 APPROVED WITH CHANGES 2026-07-18). Phase-1
> (PR #1637) deletes the DEAD python and is out of this plan's scope; the
> Phase-2a shell/CI one-liner rewrites (jq/awk/yq) ride their own PRs
> (2a-1/2a-2) under this same directive.

## Design

Two NEW workspace member crates (picked up by the existing `members =
["crates/*"]` glob — no members edit):

1. **`crates/aws-lambdas`** (package `tickvault-aws-lambdas`) — one shared
   `src/lib.rs` (env config, IST time helpers, SNS publish, SSM cached read,
   log setup) + **12 `[[bin]]` targets**, one per live lambda:
   `budget_killswitch`, `telegram_webhook`, `start_watchdog`,
   `market_open_readiness`, `deploy_watchdog`, `hard_stop_guard`,
   `daily_budget_digest` (ex-heredoc, budget-guards.tf),
   `boot_heartbeat_gate` (ex-heredoc, boot-heartbeat-alarm.tf),
   `market_hours_liveness_gate` (ex-heredoc, market-hours-liveness-alarm.tf),
   `operator_control` (lambda_http; HTML via `include_str!` templates),
   `qdb_console_front` (lambda_http, API-GW v2 payload v2),
   `qdb_console_proxy` (NO aws-sdk — plain reqwest to `QDB_BASE`, preserving
   the deliberately secret-free posture). Bins stay THIN (glue only); all
   logic lives in the lib where llvm-cov counts it. Lambdas run
   `runtime = "provided.al2023"`, `architectures = ["arm64"]`,
   `handler = "bootstrap"`, built with **cargo-lambda pinned `1.9.1`**
   (installed in CI from the official release tarball pinned by sha256;
   fallback = plain `cargo build --target aarch64-unknown-linux-musl` +
   `mv <bin> bootstrap && zip` — identical artifact contract) on the
   already-native `ubuntu-24.04-arm` runners (deploy-aws.yml precedent).
   New exact-pinned workspace deps (operator-approved under the standing
   full-authority instruction; smithy-family-matched to the lock —
   `cargo deny` duplicate check in 2b-1): `lambda_runtime`, `lambda_http`,
   `aws-sdk-ec2`, `aws-sdk-cloudwatch`, `aws-sdk-costexplorer`,
   `aws-sdk-eventbridge`, `aws-sdk-lambda`. HMAC via the already-pinned
   `aws-lc-rs`; HTTP via workspace `reqwest`.
2. **`crates/tickvault-logs-mcp`** — the MCP server rewrite: hand-rolled
   JSON-RPC 2.0 stdio loop (serde_json/tokio only — zero new deps),
   protocol `2024-11-05`, methods `initialize` / `tools/list` /
   `tools/call` / `notifications/initialized` exactly as server.py pins.
   **14-tool parity, byte-identical tool names + input_schema JSON**:
   tail_errors, list_novel_signatures, summary_snapshot, triage_log_tail,
   signature_history (`_signature_hash` algo bit-for-bit — triage-seen
   dedup state must not break), find_runbook_for_code, questdb_sql,
   grep_codebase, run_doctor, git_recent_log, tickvault_api, docker_status,
   app_log_tail, cloudwatch_logs (3-path fallback re-implemented; SigV4
   decided in-PR: aws-sdk-cloudwatchlogs vs aws-lc-rs hand-roll;
   secrets-never-logged ratcheted). Plus `--self-test`, `_is_resolved()`
   placeholder rejection + `config/claude-mcp-endpoints.toml` fallback,
   identical `{ok:false,...}` error shapes. `rmcp` was considered and
   rejected as primary (dep tree + protocol-negotiation parity risk);
   recorded as the upgrade path.
3. **Terraform packaging** — prebuilt zips, never archive_file over
   source_dir: a `build-lambdas` job (`ubuntu-24.04-arm`) in
   terraform-apply.yml produces per-bin `target/lambda/<bin>/bootstrap.zip`,
   downloaded into `deploy/aws/terraform/.lambda-zips/<name>/bootstrap.zip`
   (gitignored — binaries NEVER committed). `source_code_hash` =
   `filebase64sha256()` of a **deterministic source-digest file** (sha256
   over the crate src tree + Cargo.lock slice), NOT the zip hash — Rust
   builds are not bit-reproducible and terraform-apply gates on
   `plan -detailed-exitcode`; a fresh-zip hash would drift every run. The 3
   heredoc lambdas' inline `source {}` blocks are deleted and pointed at
   prebuilt zips (`handler` `index.handler` → `bootstrap`).
4. **CI** — `cargo test -p tickvault-aws-lambdas` (2b-1) and
   `-p tickvault-logs-mcp` (2c) land as ci.yml Test-lane **MATRIX ENTRIES**
   appended to the hardcoded `crate:` list (ci.yml:237) — explicitly NOT new
   jobs, so the all-green `needs:` list is untouched and no
   merge-gate-lock §5 REJECT row triggers.

## Edge Cases

- **Terraform drift from non-reproducible zips:** the source-digest
  `source_code_hash` updates the function only when lambda SOURCE changes;
  the tf comment states honestly that the update key is source identity,
  not artifact identity.
- **aws-smithy family duplication:** the workspace pins aws-sdk-s3
  `=1.138.0` to keep ONE smithy family in Cargo.lock — the new aws-sdk-*
  crates are selected at lock-family-matching versions (max_stable may be
  too new), resolved in 2b-1 with the `cargo deny` duplicate check.
- **All-lambdas daily-fire reality:** deploy/start watchdogs, gates and
  telegram fire every trading day — 2b-2 deploys on a non-trading window;
  telegram-webhook gets a canary SNS publish post-apply (the existing
  `make test-telegram` path).
- **operator-control (2078 LoC, 150 tests)** is the operator's ONLY control
  surface — highest single rewrite risk, staged LAST among lambdas (2b-3)
  with golden HTML snapshot tests and instant tf rollback.
- **MCP `signature_history` hashing:** the signature algorithm must match
  server.py bit-for-bit or `.claude/state/triage-seen.jsonl` dedup state
  silently resets — parity harness pins it.
- **`.mcp.json` launch mode for the Rust bin** (cargo run cold-build
  latency vs stale prebuilt binary) is an open Unknown decided with the
  coordinator inside 2c — never guessed here.
- **Missing-result/None-rendering and Fraction-exactness classes** belong
  to the 2a PRs (all-green jq, coverage-gate scaled-integer math) and are
  pinned there; this plan's crates must not regress them.
- **Release profile inheritance:** lambda bins inherit `panic = "abort"`,
  thin-lto, strip — acceptable for lambdas; a handler panic aborts the
  invoke (Lambda retries per trigger semantics), never a silent hang.

## Failure Modes

- **cargo-lambda unavailable/broken in CI** → pinned-sha tarball install
  fails LOUDLY; documented fallback is plain cargo musl build + zip with
  the identical `bootstrap` contract — never an unpinned `cargo install`.
- **A swapped lambda misbehaves live** → terraform re-apply of the previous
  commit restores the python zip atomically (runtime+zip swap in one
  apply); handler.py deletions ride the same commit so revert restores
  both. The telegram canary catches a dead alert path immediately.
- **Coverage floor breach on the new crates** → floors are
  MEASURED-then-ratcheted (llvm-cov run first, floor set at the measured
  value — never guessed), so a floor failure is a real regression, not a
  guess artifact.
- **MCP parity divergence** → the parity harness diff is the gate: no
  `.mcp.json` swap until `tools/list` diffs EMPTY and per-tool transcripts
  diff clean (timestamp-class allowlist only); a divergence found post-swap
  rolls back by flipping `.mcp.json` (revert).
- **Spurious terraform applies** → prevented by design (source-digest hash);
  if the digest script itself breaks, `plan -detailed-exitcode` surfaces
  the drift loudly in the plan job before any apply.
- **Gate-8 (22-test-type check) unsatisfiable for the lambda crate** →
  verified IN-PR during 2b-1 before any dependent PR; DHAT is NOT required
  (cold-path crate — the dhat ci job stays scoped to the existing crates).

## Test Plan

- **Port all 386 python lambda tests 1:1** as Rust `#[test]`s on pure
  functions (the handlers are already factored pure-vs-boto3): 150
  operator-control, 61 console-front, 37 telegram, 34 hard-stop, 31+31
  proxy/start-watchdog, 16 market-open-readiness, 15 deploy-watchdog,
  11 budget-killswitch — plus the 3 ex-heredoc bins gain their first tests.
- **One golden-event integration test per bin** feeding recorded
  SNS/EventBridge/API-GW JSON event shapes.
- **MCP parity harness** `scripts/mcp-parity-check.sh`: pipes the SAME
  JSON-RPC transcript (initialize → tools/list → one tools/call per tool
  against fixture log dirs) to `python3 server.py` and the Rust bin, diffs
  normalized outputs; `tools/list` must diff EMPTY; run in CI
  (validate-automation) AND one live side-by-side session
  (`tickvault-logs-rs` second entry) with evidence pasted in the PR.
- **Coverage:** two new `quality/crate-coverage-thresholds.toml` entries,
  MEASURED-then-ratcheted; `coverage_threshold_lockdown.rs` (pins "all six
  workspace crates") amended in 2b-1 and 2c alongside each entry; thin-bin
  rule keeps `[[bin]]` main.rs glue out of the covered surface.
- **Gate-8 satisfiability** for `tickvault-aws-lambdas` verified in-PR
  (2b-1); scoped runs per testing-scope.md
  (`cargo test -p tickvault-aws-lambdas` / `-p tickvault-logs-mcp`); fmt +
  scoped clippy `-D warnings` clean; `cargo deny` duplicate check green.
- **Guard amendments tested by running the amended guards**: each guard
  edit (budget_killswitch_wiring.rs, telegram_lambda_house_style_guard.rs,
  aws_infra_wiring.rs, deploy_provenance_guard.rs,
  cloudwatch_agent_glob_guard.rs, tickvault_logs_mcp_guard.rs,
  claude_session_bootstrap_guard.rs, claude_mcp_endpoints_config_guard.rs)
  lands in the SAME PR as the code it re-pins and runs green there.

## Rollback

- **Per-PR revert** — the sequence is serial (charter §H rule 12), each PR
  ≤30 files, each independently squash-revertable.
- **Lambda PRs (2b-1..2b-3):** terraform keeps the python zips deployable
  until each lambda's own cutover PR — a revert of the PR restores
  handler.py + the archive_file packaging in the same commit, and a
  terraform re-apply of the previous commit restores the live function.
- **MCP PR (2c):** the `.mcp.json` swap is the LAST step and is trivially
  reversible (flip `command` back to `python3 server.py`); server.py +
  test_placeholder_fallback.py are deleted only after parity evidence
  lands (same PR — rollback = revert of that PR).
- **No schema/data migration anywhere** in this phase; config additions are
  serde-defaulted or CI-only.

## Observability

- **No new ErrorCode variants are needed**: lambdas and the MCP server run
  OUTSIDE the app process — their observability surfaces are CloudWatch
  lambda logs/metrics + the SNS→Telegram chain they themselves implement,
  not the in-app `error_code.rs` taxonomy. The existing lambda-facing
  alarms (`tv_binary_main_sha_mismatch`, boot-heartbeat, market-hours
  liveness window gates) keep their metric names and semantics — the
  rewrite is implementation-language-only per lambda.
- **validate-automation checks amended in 2c**:
  `scripts/validate-automation.sh:93-124` re-points the MCP self-test +
  placeholder-fallback checks at the Rust bin (the `python3 server.py
  --self-test` and `test_placeholder_fallback.py` invocations die with
  server.py); the mcp guard cargo-test invocation stays.
- **Telegram canary** post-apply on the telegram-webhook swap (2b-2) — the
  one lambda whose failure silences every other page.
- **Secrets discipline ratcheted**: tickvault_logs_mcp_guard.rs (rewritten
  in 2c) pins the Rust tool registry, stdio loop, dep allowlist, and the
  secrets-never-logged contract for cloudwatch_logs/tickvault_api.
- **Deploy provenance unchanged**: deploy_provenance_guard.rs amendments
  keep the binary-sha SSM chain pins intact through the
  operator-control/deploy-watchdog rewrites.

## Plan Items

- [ ] **2b-1 — `crates/aws-lambdas` skeleton + 3 ex-heredoc bins + budget-killswitch pilot** (workspace dep pins; ci.yml Test-lane matrix entry `aws-lambdas`; coverage floor measured-then-ratcheted + lockdown amend; terraform-apply.yml `build-lambdas` job; tf swaps for the 3 heredocs + budget-killswitch; gate-8 satisfiability verified in-PR)
  - Files: crates/aws-lambdas/Cargo.toml, crates/aws-lambdas/src/lib.rs, crates/aws-lambdas/src/bin/budget_killswitch.rs, crates/aws-lambdas/src/bin/daily_budget_digest.rs, crates/aws-lambdas/src/bin/boot_heartbeat_gate.rs, crates/aws-lambdas/src/bin/market_hours_liveness_gate.rs, Cargo.toml, Cargo.lock, .github/workflows/ci.yml, .github/workflows/terraform-apply.yml, quality/crate-coverage-thresholds.toml, crates/storage/tests/coverage_threshold_lockdown.rs, deploy/aws/terraform/budget-guards.tf, deploy/aws/terraform/boot-heartbeat-alarm.tf, deploy/aws/terraform/market-hours-liveness-alarm.tf, deploy/aws/terraform/budget-killswitch-lambda.tf
  - Tests: test_budget_killswitch_event_parse, test_daily_budget_digest_render, test_boot_heartbeat_gate_modes, test_market_hours_liveness_gate_modes, budget_killswitch_wiring (amended), coverage_threshold_lockdown (amended)
- [ ] **2b-2 — 5 event lambdas** (telegram-webhook, start-watchdog, market-open-readiness, deploy-watchdog, hard-stop-guard: bins + tf swaps + handler.py/test_handler.py deletes + 133 python tests ported + Makefile lambda-test → cargo test)
  - Files: crates/aws-lambdas/src/bin/telegram_webhook.rs, crates/aws-lambdas/src/bin/start_watchdog.rs, crates/aws-lambdas/src/bin/market_open_readiness.rs, crates/aws-lambdas/src/bin/deploy_watchdog.rs, crates/aws-lambdas/src/bin/hard_stop_guard.rs, deploy/aws/terraform/telegram-webhook-lambda.tf, deploy/aws/terraform/start-watchdog-lambda.tf, deploy/aws/terraform/market-open-readiness-lambda.tf, deploy/aws/terraform/deploy-watchdog-lambda.tf, Makefile
  - Tests: test_telegram_render_alarm_fold, test_start_watchdog_curfew_modes, test_market_open_readiness_verdicts, test_deploy_watchdog_sha_compare, test_hard_stop_guard_window, telegram_lambda_house_style_guard (amended), aws_infra_wiring (amended), deploy_provenance_guard (amended), cloudwatch_agent_glob_guard (amended)
- [ ] **2b-3 — HTTP lambda pair + operator-control** (questdb-console-front + proxy + operator-control bins, tf swaps, ci.yml:611-619 python unittest lane → cargo test, gate-matrix invocation kept, 242 python tests ported, golden HTML snapshots)
  - Files: crates/aws-lambdas/src/bin/operator_control.rs, crates/aws-lambdas/src/bin/qdb_console_front.rs, crates/aws-lambdas/src/bin/qdb_console_proxy.rs, deploy/aws/terraform/questdb-console.tf, deploy/aws/terraform/operator-control-lambda.tf, .github/workflows/ci.yml
  - Tests: test_operator_control_hmac_auth, test_operator_control_html_golden, test_qdb_console_front_link_token, test_qdb_console_proxy_readonly_sql_gate, deploy_provenance_guard (amended)
- [ ] **2c — `crates/tickvault-logs-mcp` rewrite + guard amendments + .mcp.json swap** (14-tool parity + harness + self-test; ci.yml matrix entry `tickvault-logs-mcp`; coverage floor measured-then-ratcheted + lockdown amend; validate-automation re-point; server.py + test_placeholder_fallback.py deleted after parity evidence)
  - Files: crates/tickvault-logs-mcp/Cargo.toml, crates/tickvault-logs-mcp/src/main.rs, crates/tickvault-logs-mcp/src/tools/mod.rs, scripts/mcp-parity-check.sh, .mcp.json, scripts/validate-automation.sh, crates/common/tests/tickvault_logs_mcp_guard.rs, crates/common/tests/claude_session_bootstrap_guard.rs, crates/common/tests/claude_mcp_endpoints_config_guard.rs, .github/workflows/ci.yml, quality/crate-coverage-thresholds.toml, crates/storage/tests/coverage_threshold_lockdown.rs
  - Tests: test_tools_list_schema_parity, test_signature_hash_matches_python, test_placeholder_env_falls_back_to_toml, test_self_test_mode, tickvault_logs_mcp_guard (rewritten), claude_session_bootstrap_guard (amended), claude_mcp_endpoints_config_guard (amended)
- [ ] **2d — word-sweep + rust-only ratchet** (tree-wide `python` residue cleanup in docs/tf comments/READMEs; banned-pattern-scanner category + ratchet test forbidding re-introduction outside docs/dhan-support/ + the KEPT .claude/skills/dhanhq/ vendor examples per operator ruling)
  - Files: .claude/hooks/banned-pattern-scanner.sh, crates/common/tests/rust_only_python_ban_guard.rs, deploy/aws/terraform/user-data.sh.tftpl, README/doc files with residual wording
  - Tests: rust_only_python_ban_guard (new ratchet), banned-pattern-scanner self-check

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` — the
15-row 100% Guarantee Matrix and the 7-row Resilience Demand Matrix apply to
every item above. Honest envelope: 100% inside the tested envelope, with
ratcheted regression coverage — 386 python tests ported 1:1 plus golden-event
and parity harnesses; coverage floors measured-then-ratcheted (never
guessed); every guard amendment lands in the same PR as the code it re-pins;
the tick hot path, the WS/aggregator chain, and every QuestDB DEDUP key are
UNTOUCHED by this phase (lambdas + MCP are out-of-process, cold-path
surfaces). NOT claimed: bit-reproducible lambda zips (the source-digest hash
is the honest drift key), lambda cold-start numbers before the first live
invoke, or the `.mcp.json` launch-mode latency (Unknown — decided with the
coordinator in 2c).
