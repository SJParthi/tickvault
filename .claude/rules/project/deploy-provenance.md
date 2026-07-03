# Deploy Provenance — B9 (git SHA stamped end-to-end)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file.
> **Operator directive (2026-07-03, B9):** the git commit SHA must be visible
> at every provenance surface — the running binary, the boot Telegram, the
> operator-portal footer, terraform outputs — and a stale binary (≠ main HEAD
> for >24h) must page.
> **Companion code:** `crates/common/build.rs`, `crates/common/src/build_info.rs`,
> ratchet `crates/common/tests/deploy_provenance_guard.rs`.

## §0. The chain (single diagram)

```
TICKVAULT_BUILD_GIT_SHA (deploy CI, exact) / git rev-parse HEAD (dev) / "unknown" (no git)
        └─▶ crates/common/build.rs ─▶ env!("TICKVAULT_GIT_SHA")
                └─▶ build_info::BUILD_GIT_SHA (+ build_git_sha_short())
                        ├─▶ GET /health  "git_sha" field        (crates/api)
                        └─▶ boot Telegram "Build: <short7>"     (crates/core)

deploy-aws.yml (AFTER verified swap, non-fatal)
        └─▶ SSM /tickvault/prod/deploy/binary-git-sha = GITHUB_SHA
                ├─▶ operator-portal footer:
                │     "binary <b7> · portal <p7> · main <m7>"
                │     (binary = SSM · portal = PORTAL_GIT_SHA env from
                │      terraform var.portal_git_sha, CI-set via
                │      TF_VAR_portal_git_sha · main = GitHub API HEAD)
                └─▶ deploy-watchdog Lambda: tv_binary_main_sha_mismatch
                      metric (Tickvault/Prod, host=tickvault-prod)
                        └─▶ alarm tv-<env>-binary-sha-stale
                              (Minimum ≥ 1 over 86400s, notBreaching)
                              └─▶ SNS tv_alerts ─▶ Telegram

terraform outputs: portal_git_sha + binary_git_sha_ssm_param
```

## §1. Honest envelope

- The SHA is **exact for deploy-CI-built artifacts**: the deploy workflow's
  two build steps set the **dedicated `TICKVAULT_BUILD_GIT_SHA` env var**
  from `github.sha` (the only path that produces the prod binary). The
  build script deliberately does NOT read GitHub's own `GITHUB_SHA` — a
  `rerun-if-env-changed` registration on it changes every commit in EVERY
  workflow and invalidated the restored target cache (full workspace
  rebuild per commit in Build & Verify). Only the deploy build sets the
  dedicated var; other workflows keep their caches valid.
- **Local dev builds** fall back to `git rev-parse HEAD` at build-script run
  time and may lag HEAD until the build script re-runs
  (`rerun-if-changed` on `.git/HEAD` PLUS on the resolved branch-ref file —
  a plain `git commit` rewrites only the branch ref, not HEAD — bound the
  staleness; worktrees where `.git` is a file remain a documented skip).
  Builds without git embed the literal `unknown` — never garbage
  (40-lowercase-hex validated at build time).
- The 24h alarm evaluates `Minimum ≥ 1` over one 86400s period at the
  watchdog's ~2-3 samples/weekday cadence — it **approximates** ">24h
  stale", it is not an exact wall-clock measurement. Missing data (box off,
  weekends, unknown-sha skip) never pages (`notBreaching`).
- The watchdog **skips** publishing when either SHA is unknown — no false
  signal on uncertainty (mirrors the `is_stale` no-false-OK contract).
- The portal GET can never fail or hang on provenance: all three lookups
  are individually fail-soft to `unknown`; the GitHub call is 3s-bounded +
  60s cached.
- The boot Telegram `Build:` short-SHA line is a **conscious
  operator-approved override** of the "no version numbers in body" Telegram
  commandment (B9 directive 2026-07-03) — one line, one purpose: "WHICH
  code booted?".
- No new ErrorCode: this feature adds no Rust `error!` emit site. No
  hot-path code is touched (boot/API/deploy-time only; zero per-tick cost).
- **Staging caveat:** the deploy workflow writes the SSM param at the
  HARDCODED `/tickvault/prod/deploy/binary-git-sha` path while terraform
  reads `/tickvault/${var.environment}/deploy/binary-git-sha`
  (operator-control-lambda.tf + deploy-watchdog-lambda.tf). Prod is the
  single real environment, so the two match today — but they MUST stay in
  lockstep, and a future staging environment would need the workflow's
  put-parameter step parameterized on the target environment.

## §1.1. 2026-07-03 adversarial-review fixes (post-impl 3-agent pass)

| Sev | Finding | Fix |
|---|---|---|
| HIGH | `rerun-if-env-changed` on `GITHUB_SHA` invalidated every workflow's restored target cache (GITHUB_SHA changes per commit, set everywhere) — full workspace rebuild per commit | Dedicated `TICKVAULT_BUILD_GIT_SHA` env var, set ONLY by the deploy build steps in `deploy-aws.yml`; build.rs never reads/registers GITHUB_SHA (ratcheted) |
| MEDIUM | `rerun-if-changed=.git/HEAD` misses a plain `git commit` (only the branch ref file changes) — local dev could embed a stale sha | build.rs also registers the resolved branch-ref file (`ref: ` prefix in HEAD) when it exists; worktrees stay a documented skip |
| MEDIUM | Portal footer spliced un-sanitized SHAs into HTML (poisoned SSM param / GitHub response = markup injection) | `_safe_provenance_sha` (`re.fullmatch(r"[0-9a-f]{7,40}")` → else `unknown`) on all three SHAs + `html.escape` on the assembled line (defense in depth) |
| MEDIUM | Workflow-vs-terraform SSM path (hardcoded prod vs `var.environment`) could silently diverge | Lockstep comments at all three sites + the staging caveat above |
| LOW | `_main_sha` module cache served the last GitHub value indefinitely on sustained failure | Hard 600s max-age: stale-refresh past 600s returns `unknown` (60s fresh TTL kept) |
| LOW | SSM provenance write recorded `GITHUB_SHA` unvalidated | `[[ "${GITHUB_SHA}" =~ ^[0-9a-f]{40}$ ]]` guard; non-hex → warning, still non-fatal |

## §2. What a PR that violates this looks like (REJECT)

- Removes the `aws ssm put-parameter …/deploy/binary-git-sha` step from
  `deploy-aws.yml` (blinds the portal footer + the staleness alarm).
- Makes that SSM write step FATAL (a provenance-recording failure must
  never fail or roll back a verified-healthy deploy).
- Drops the `git_sha` field from `/health` or the `Build:` line from the
  StartupComplete Telegram.
- Deletes/weakens the `tv-<env>-binary-sha-stale` alarm or the
  `tv_binary_main_sha_mismatch` publish in the deploy-watchdog.
- Makes the portal footer lookups blocking/fatal (GET must render even
  with SSM + GitHub both down).
- Embeds an UNVALIDATED sha (anything not 40-lowercase-hex must degrade to
  `unknown`).
- Re-introduces a `rerun-if-env-changed` registration on `GITHUB_SHA` in
  any build script (kills the CI target cache in every workflow), or drops
  `TICKVAULT_BUILD_GIT_SHA` from the deploy build steps (a cached deploy
  build could embed a stale sha).
- Renders a provenance sha in the portal footer without the hex validation
  + html-escape (markup injection surface).

All of the above are pinned by the build-failing ratchet
`crates/common/tests/deploy_provenance_guard.rs`.

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/build.rs` or `crates/common/src/build_info.rs`
- `crates/api/src/handlers/health.rs`
- `crates/core/src/notification/events.rs` (the StartupComplete arm)
- `.github/workflows/deploy-aws.yml` or `.github/workflows/terraform-apply.yml`
- `deploy/aws/lambda/operator-control/handler.py`
- `deploy/aws/lambda/deploy-watchdog/handler.py`
- `deploy/aws/terraform/{operator-control-lambda,deploy-watchdog-lambda,oidc,variables,outputs}.tf`
- Any file containing `TICKVAULT_GIT_SHA`, `BUILD_GIT_SHA`, `binary-git-sha`,
  `tv_binary_main_sha_mismatch`, or `portal_git_sha`
