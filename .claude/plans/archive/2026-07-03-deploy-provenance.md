# Implementation Plan: B9 Deploy Provenance

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — B9 deploy-provenance directive 2026-07-03, this session
**Branch:** `claude/deploy-provenance`
**Changed crates:** `common` (crates/common), `api` (crates/api), `core` (crates/core)
**Changed non-crate surfaces:** `.github/workflows/deploy-aws.yml`, `.github/workflows/terraform-apply.yml`, `deploy/aws/lambda/operator-control/handler.py`, `deploy/aws/lambda/deploy-watchdog/handler.py`, `deploy/aws/terraform/{operator-control-lambda.tf,deploy-watchdog-lambda.tf,variables.tf,outputs.tf,oidc.tf}`

> Guarantee matrices: carried by cross-reference to
> `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row,
> mandatory). All 15 + 7 rows apply to every item in this plan; the
> item-specific proofs are the ratchet test
> `crates/common/tests/deploy_provenance_guard.rs`, the unit tests listed
> per item below, and the honest envelope in the Observability section.

## Design

The git commit SHA becomes a first-class provenance signal across four
surfaces, chained end-to-end:

```
TICKVAULT_BUILD_GIT_SHA (deploy CI) ──▶ crates/common/build.rs ──▶ env!("TICKVAULT_GIT_SHA")
                                              │
                    ┌─────────────────────────┤
                    ▼                         ▼
        GET /health "git_sha" field    boot Telegram "Build: <short7>"
                    (crates/api)              (crates/core events.rs)

deploy-aws.yml (post-verified-swap) ──▶ SSM /tickvault/prod/deploy/binary-git-sha
                    │
    ┌───────────────┴────────────────────────────────┐
    ▼                                                ▼
operator-portal footer                    deploy-watchdog Lambda
"binary abc1234 · portal def5678          tv_binary_main_sha_mismatch metric
 · main xyz1234"                          ──▶ 24h CloudWatch alarm
                                              tv-prod-binary-sha-stale
terraform: var.portal_git_sha (set by CI TF_VAR) → Lambda env PORTAL_GIT_SHA
           + outputs portal_git_sha / binary_git_sha_ssm_param
```

1. **Binary embed** — new `crates/common/build.rs` resolves the SHA at
   compile time: dedicated `TICKVAULT_BUILD_GIT_SHA` env (set ONLY by the
   deploy workflow's build steps — exact for shipped artifacts, and other
   workflows' target caches stay valid because they never set it) →
   `git rev-parse HEAD` (local dev) → `"unknown"` (fail-soft, never
   garbage; 40-lowercase-hex validated). Exposed as
   `tickvault_common::build_info::BUILD_GIT_SHA` const +
   `build_git_sha_short()` (7-char prefix, no panic path).
2. **/health** — `HealthResponse` gains `git_sha: &'static str`.
3. **Boot Telegram** — the `StartupComplete` `to_message()` arm appends a
   `Build: <short7>` line. Variant signature UNCHANGED (no callsite churn).
4. **SSM provenance param** — `deploy-aws.yml` writes
   `/tickvault/prod/deploy/binary-git-sha` = `GITHUB_SHA` AFTER the
   verified swap; non-fatal on failure. OIDC deploy role gains
   `ssm:PutParameter` on exactly that param ARN.
5. **Portal footer** — operator-control Lambda renders
   `binary <b7> · portal <p7> · main <m7>` in a muted footer on the GET
   HTML. Sources: SSM param (binary), `PORTAL_GIT_SHA` env from terraform
   `var.portal_git_sha` wired from CI `TF_VAR_portal_git_sha=${{ github.sha }}`
   (portal), GitHub API main HEAD via the existing operator token (main,
   60s module-global cache). Each lookup independently fail-soft to
   `unknown` — GET can never 5xx or hang on provenance.
6. **24h staleness alarm** — deploy-watchdog computes pure
   `binary_mismatch_value(binary_sha, desired_sha)` (1.0 known-unequal /
   0.0 known-equal / None skip) and publishes
   `Tickvault/Prod tv_binary_main_sha_mismatch {host=tickvault-prod}` via
   direct PutMetricData (bypasses the EMF allowlist by design). New alarm
   `tv-prod-binary-sha-stale`: Minimum ≥ 1 over one 86400s period,
   `notBreaching` on missing (box off / weekends never page).
7. **Ratchet** — `crates/common/tests/deploy_provenance_guard.rs`
   source-scans all wiring points; rule file
   `.claude/rules/project/deploy-provenance.md` documents the chain.

## Edge Cases

- **Local dev build without `TICKVAULT_BUILD_GIT_SHA`**: build.rs shells
  `git rev-parse HEAD`; stale until the build script re-runs
  (rerun-if-changed on `.git/HEAD` AND on the resolved branch-ref file —
  a plain commit only rewrites the ref file — plus rerun-if-env-changed on
  `TICKVAULT_BUILD_GIT_SHA` bound the staleness; worktrees where `.git` is
  a file remain a documented skip).
- **No git / no .git dir (vendored source, docker builds)**: SHA =
  `"unknown"` — build never fails, value never garbage (40-hex validated).
- **Short SHA slicing on `"unknown"`**: `get(..7)` falls back to the full
  string (`"unknown"` is 7 chars anyway); no panic path.
- **Telegram port-leak test vs random SHA content**: a commit SHA can
  contain `9000`/`3000` as substrings; the existing
  `test_startup_complete_live_message` port assertions are applied to the
  message with the `Build:` line stripped so the ratchet stays
  deterministic across commits.
- **SSM param missing (first deploy before workflow change lands)**:
  portal shows `binary unknown`; watchdog metric is SKIPPED (None) — no
  false alarm.
- **GitHub API blip in portal `_main_sha()`**: `unknown` after ≤3s
  timeout, cached 60s — GET latency bounded.
- **Mixed-case / 40-vs-7-char SHA comparison in the watchdog**: values
  normalized to lowercase first-7 before compare.
- **Deploy skipped off-hours (`DEPLOY_SKIP_REASON`)**: the SSM write step
  is gated the same way as the success Telegram — no phantom provenance.
- **Weekend / box off**: alarm `treat_missing_data = notBreaching` — no
  page while the watchdog isn't sampling mismatch=1.

## Failure Modes

- **build.rs git invocation fails** (no git binary, not a repo):
  fall through to `"unknown"`; compile proceeds; unit test accepts
  `"unknown"` as legal.
- **SSM put-parameter fails post-deploy** (throttle, IAM drift): step
  logs a visible `WARN` but exits 0 — a provenance-recording failure must
  never roll back a healthy deploy. The next deploy retries; until then
  the portal shows the previous/unknown value.
- **Portal provenance lookups fail** (SSM down, GitHub down): all three
  helpers are individually try/except → `unknown`; the page still renders.
- **Watchdog PutMetricData fails**: wrapped in try/except; the watchdog's
  core stale-deploy dispatch job is unaffected; alarm goes missing-data
  (notBreaching) rather than false-firing.
- **Binary genuinely stale > 24h** (deploy pipeline stuck/bypassed): every
  watchdog sample in the 24h window reports 1.0 → alarm fires → SNS →
  Telegram, with a runbook pointer in the alarm description.
- **Alarm can under-fire on sparse samples**: with ~3 samples/weekday the
  Minimum-over-24h statistic approximates ">24h stale"; documented
  honestly in the alarm comment (not claimed as exact).

## Test Plan

- `crates/common/src/build_info.rs` unit tests: const shape (40-hex OR
  `unknown`), short-sha length/prefix invariants.
- `crates/api/src/handlers/health.rs`: existing serialization + version
  tests updated for the new field; NEW test asserting the JSON carries
  `"git_sha"` matching `build_info::BUILD_GIT_SHA`.
- `crates/core/src/notification/events.rs`: StartupComplete message tests
  extended — message contains `Build: ` + the short sha; port-leak
  assertions made SHA-proof.
- `crates/common/tests/deploy_provenance_guard.rs`: 10+ source-scan
  ratchets pinning build.rs, build_info, health field, events line, the
  deploy-aws.yml SSM write, portal footer format, watchdog metric name,
  terraform alarm + outputs, and the rule file's existence.
- Python pure-function tests (no CI runner, run locally):
  `deploy/aws/lambda/deploy-watchdog/test_handler.py` gains
  `binary_mismatch_value` match/mismatch/unknown cases;
  `deploy/aws/lambda/operator-control/test_handler.py` gains
  `_provenance_line` formatting tests.
- Gates: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings
  -W clippy::perf`, `cargo test --workspace` (common changed → workspace
  escalation), banned-pattern scanner, plan-gate, per-item-guarantee-check.

## Rollback

- Rust: revert the branch commits — `git_sha` field, build.rs, build_info
  module and events line are additive; no schema, no persisted state.
- Workflow: deleting the SSM put step reverts to no provenance param; the
  param itself is inert data (String, not read by the app).
- Terraform: `terraform apply` after revert destroys the alarm and drops
  the Lambda env vars; `portal_git_sha` variable default `unknown` means
  an old tfvars-less apply is still valid.
- The ratchet test intentionally makes silent partial rollback
  build-failing — a deliberate rollback must also revert
  `deploy_provenance_guard.rs` + `.claude/rules/project/deploy-provenance.md`
  in the same commit.

## Observability

- `/health.git_sha` — machine-checkable provenance on every probe.
- Boot Telegram `Build: <short7>` — operator sees what booted. NOTE: the
  Telegram commandment "no version numbers in body" is consciously
  overridden by the operator's B9 directive for this ONE `Build:` line.
- SSM `/tickvault/prod/deploy/binary-git-sha` — queryable system of
  record for "what was last successfully deployed".
- Portal footer `binary … · portal … · main …` — one-glance drift view.
- CloudWatch metric `tv_binary_main_sha_mismatch` + alarm
  `tv-prod-binary-sha-stale` (24h Minimum ≥ 1, notBreaching) → SNS
  tv_alerts → Telegram.
- Honest envelope: the SHA is EXACT for CI-built artifacts (`GITHUB_SHA`);
  local dev builds fall back to `git rev-parse HEAD` at build-script run
  time and may lag HEAD until the build script re-runs — a documented
  limitation, not hidden. The 24h alarm approximates ">24h stale" at the
  watchdog's ~2-3 samples/weekday cadence and never pages on missing data.
  No new ErrorCode is added (no new Rust `error!` emit site). No hot-path
  code is touched (all additions are boot/API/deploy-time; zero per-tick
  cost).

## Plan Items

- [x] Item 1 — Embed git SHA in the binary (build.rs + build_info module)
  - Files: crates/common/build.rs, crates/common/src/build_info.rs, crates/common/src/lib.rs
  - Tests: test_build_git_sha_is_40_hex_or_unknown, test_build_git_sha_short_is_prefix_and_max_7
- [x] Item 2 — /health reports git_sha
  - Files: crates/api/src/handlers/health.rs
  - Tests: test_health_check_git_sha_matches_build_info, test_health_response_serialization
- [x] Item 3 — Boot Telegram Build: line (no variant signature change)
  - Files: crates/core/src/notification/events.rs
  - Tests: test_startup_complete_message_contains_build_sha, test_startup_complete_live_message
- [x] Item 4 — deploy-aws.yml records deployed binary SHA to SSM post-swap (non-fatal), OIDC role gains ssm:PutParameter on the param ARN
  - Files: .github/workflows/deploy-aws.yml, deploy/aws/terraform/oidc.tf
  - Tests: deploy_workflow_records_binary_sha_to_ssm (ratchet)
- [x] Item 5 — Portal provenance footer (binary · portal · main)
  - Files: deploy/aws/lambda/operator-control/handler.py, deploy/aws/lambda/operator-control/test_handler.py, deploy/aws/terraform/operator-control-lambda.tf
  - Tests: portal_footer_renders_provenance_line (ratchet)
- [x] Item 6 — terraform portal_git_sha variable + provenance outputs + CI TF_VAR wiring
  - Files: deploy/aws/terraform/variables.tf, deploy/aws/terraform/outputs.tf, .github/workflows/terraform-apply.yml
  - Tests: terraform_outputs_expose_provenance (ratchet)
- [x] Item 7 — binary≠main mismatch metric + 24h stale CloudWatch alarm
  - Files: deploy/aws/lambda/deploy-watchdog/handler.py, deploy/aws/lambda/deploy-watchdog/test_handler.py, deploy/aws/terraform/deploy-watchdog-lambda.tf
  - Tests: watchdog_publishes_binary_mismatch_metric (ratchet)
- [x] Item 8 — Ratchet guard + rule file
  - Files: crates/common/tests/deploy_provenance_guard.rs, .claude/rules/project/deploy-provenance.md
  - Tests: rule_file_exists_and_documents_chain

## 2026-07-03 adversarial-review fixes (post-impl 3-agent pass)

All findings fixed on this branch (see `.claude/rules/project/deploy-provenance.md` §1.1 for the full table):

- **HIGH** — CI cache-kill: `rerun-if-env-changed=GITHUB_SHA` invalidated
  every workflow's restored target cache. Fixed via the dedicated
  `TICKVAULT_BUILD_GIT_SHA` env var, set only by the deploy build steps
  (both the shipped and smoke binaries); build.rs never reads/registers
  GITHUB_SHA (negative ratchet in `deploy_provenance_guard.rs`).
- **MEDIUM** — local-dev stale-sha rerun gap: a plain `git commit` doesn't
  rewrite `.git/HEAD`; build.rs now also registers the resolved branch-ref
  file for `rerun-if-changed`.
- **MEDIUM** — portal footer XSS hardening: `_safe_provenance_sha`
  (`re.fullmatch(r"[0-9a-f]{7,40}")` → else `unknown`) on all three SHAs +
  `html.escape` of the assembled footer line (defense in depth); unittest
  cases prove `<script` renders as `unknown`.
- **MEDIUM** — hardcoded prod SSM path vs terraform `var.environment`:
  lockstep comments added at the workflow step + both Lambda tf env blocks
  + the rule-file staging caveat.
- **LOW** — `_main_sha` unbounded stale cache: hard 600s max-age; a failed
  refresh past 600s returns `unknown` (60s fresh TTL kept).
- **LOW** — SSM provenance write now hex-validates `GITHUB_SHA`
  (`^[0-9a-f]{40}$`) before put-parameter; non-hex → warning, non-fatal.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Deploy CI build with TICKVAULT_BUILD_GIT_SHA set | binary embeds the exact 40-hex sha |
| 2 | Local dev build, git available | binary embeds `git rev-parse HEAD` |
| 3 | Build without git/.git | binary embeds `unknown`; build succeeds |
| 4 | Deploy verified → SSM write fails | deploy stays green; WARN visible |
| 5 | Portal GET with SSM+GitHub down | footer shows `unknown` values; page renders |
| 6 | Binary stale vs main for a full day | alarm fires once → Telegram |
| 7 | Weekend, box stopped | metric missing → alarm notBreaching, silent |
| 8 | Sha prefix ambiguity (7-char) | compare normalized lowercase first-7 both sides |
