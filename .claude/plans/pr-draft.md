# Pull Request Draft — Zero Tick Loss + AWS Deployment Readiness

> This file is the pre-written PR body for the
> `claude/websocket-zero-tick-loss-nUAqy` branch. Open the PR
> when you're ready by running either:
>
> ```bash
> gh pr create \
>   --base main \
>   --head claude/websocket-zero-tick-loss-nUAqy \
>   --title "Zero tick loss + Phase 8 AWS deployment readiness" \
>   --body-file .claude/plans/pr-draft.md
> ```
>
> ... or paste the body below into the GitHub web UI.

## Summary

128 commits across 10 sessions hardening the zero-tick-loss
guarantee and making the repo AWS-deployment-ready for the May 1
go-live.

## What's in this PR

### Zero tick loss (sessions 1-3)

- **4-layer tick persistence chain** proven survivable to 1M ticks
  at a disconnected QuestDB
  (`chaos_questdb_full_session_zero_tick_loss`)
- **Ring buffer → disk spill → DLQ → process-restart replay** all
  wired + tested
- **Pool watchdog** now actually polled in main.rs (was dormant,
  fixed in S4)
- **Graceful shutdown** now actually called on SIGTERM (was
  dormant, fixed in S4)
- **Slow-boot synth writer** — backfilled ticks now persist in
  slow-boot mode (was broken, fixed in S5)
- **Instrument registry lookup** for backfill fetcher — correct
  F&O instrument type per contract (was wrong, fixed in S5)
- **Governor rate limiter** on backfill (5/sec per Dhan limit)
- **DEDUP invariants** locked by 8 proptests (~10k random cases)

### Mechanical enforcement (sessions 2, 6)

- **12 pre-push gates** including new S6 gates:
  - G1 pub-fn wiring guard (blocks dormant pub fns)
  - G3/G4 boot-symmetry guard (blocks slow-boot blind spots)
- **9 pre-commit gates** including new commit-time invariants:
  - DEDUP uniqueness property test
  - Depth byte-offset invariants
  - Bible-deletion lockdown
- **33 catalogued metrics** under lockdown (can't silently delete)
- **14 Grafana alerts** + **5 CloudWatch alarms**
- **8 Dhan ground-truth locked facts** from support tickets

### Sandbox-only gate (session 6)

- `config/base.toml` pins `sandbox_only_until = 2026-06-30`
- Boot-time gate in `crates/app/src/main.rs` panics if Live is
  requested before that date
- `config/staging.toml` has cutoff `2099-12-31` — staging cannot
  ever accidentally promote to Live
- 11 tests lock the sandbox behavior

### AWS deployment ready (sessions 7-9)

- **Complete Terraform stack** (`deploy/aws/terraform/`)
  - ap-south-1 c7i.xlarge, Rs 4,981/month under the Rs 5,000 cap
  - VPC + EC2 + EIP + IAM + SNS + 4 EventBridge schedules
  - S3 cold bucket with 30d→365d→5yr SEBI lifecycle
  - OIDC role for GitHub Actions (minimum privilege)
  - 5 CloudWatch alarms → Telegram
- **GitHub Actions deploy workflow** (`deploy-aws.yml`)
  - OIDC auth, market-hours guard, smoke test gate,
    60-second post-deploy monitor, auto-rollback
- **Smoke test binary** (`crates/app/src/bin/smoke_test.rs`)
  - 5 fail-safe checks, under 500ms, called by deploy workflow
- **dlt-doctor CLI** (`crates/app/src/bin/dlt_doctor.rs`)
  - Auto-triage bundle on CRITICAL alerts (5 probes, 3 formats)
- **Nightly dep-freshness workflow** (`dep-freshness-nightly.yml`)
  - Auto-PR for patches, GitHub issue for minor/major drifts
- **Two runbooks** (`docs/runbooks/aws-*.md`)
  - First-time setup (220 lines, 7 phases)
  - Disaster recovery (280 lines, 11 symptom sections)
- **12 Makefile aws-* targets** — one-command bootstrap

### Dependency hygiene (sessions 7, 9, 10)

- **18 dep bumps** across S7 + S10 (tokio, governor, chrono,
  futures-util, papaya, rtrb, quanta, arc-swap, tower, tower-http,
  chrono-tz, anyhow, memmap2, signal-hook, tokio-test, bytes, csv,
  proptest)
- **Dep freshness scanner** + parser fix for pre-release versions
  (64 deps fresh, 5 MINOR drifts deferred)
- **CVE verification** against 7 RustSec advisories — **all
  patched** (rustls 2024-0336/0399, time, idna, ring, hyper, tokio)
- **Tech Stack Bible DELETED** — Cargo.toml is now the executable
  single source of truth

### Dhan verification (sessions 8, 10)

- **12 invariant groups verified** against the DhanHQ-py v2.2.0
  SDK via WebFetch on raw.githubusercontent.com
- **1 critical drift caught**: 200-level depth URL path — SDK uses
  root `/`, but Dhan support ticket #5519522 mandates
  `/twohundreddepth`. Our rule is CORRECT; SDK is stale. Vindicates
  session 2's `dhan_locked_facts.rs` lockdown.
- **1 possible rule staleness flagged**: `PRE_OPEN` AMO value
  missing from SDK (operator cross-check needed against Dhan docs)
- **2 items unverified**: rate limits (runtime) + option chain
  `client-id` header (SDK-internal). Both already locked by tests.

### Test counts

- ~3300 tests passing total
- ~180 new tests added across sessions 1-10
- 17 AWS infra lockdown tests (session 10)
- 8 DEDUP property tests (~10k random cases each)
- 13 depth invariant tests (~8k random cases each)
- 1M-tick chaos test running in under 6 seconds

### What I explicitly did NOT do

- **No `terraform apply`** — needs your AWS credentials
- **No GitHub Actions workflow runs** — they fire on tag push or
  manual dispatch
- **No Dhan static IP registration** — needs the EIP from the
  first Terraform apply + 7-day cooldown awareness
- **No release binary built in this session** (the CI workflow
  builds + size-checks it)

## Honest open items (documented in session reports)

1. **8 Dependabot CVE alerts** — cross-checked 7 common RustSec
   advisories, all patched. Remaining 8 GHSA alerts need manual
   enumeration in the Dependabot UI (5 min of your time).
2. **Dhan docs primary verification** — `dhanhq.co` blocked by
   Cloudflare in the dev sandbox; SDK verification covered 12 of
   14 groups instead. 5 min of manual cross-check fills the gap.
3. **5 MINOR dep drifts** deferred (redis, reqwest, signal-hook-tokio,
   sd-notify, teloxide) — each needs its own review session.
4. **First `terraform apply`** — 20 minutes of your time following
   `docs/runbooks/aws-deploy.md`.

## Test plan

- [ ] CI builds the release binary and confirms < 15 MB
- [ ] CI runs the full test suite (`cargo test --workspace`)
- [ ] CI runs `cargo clippy --workspace -- -D warnings`
- [ ] CI runs `cargo audit` with fresh advisory DB
- [ ] Maintainer reviews the Terraform stack against aws-budget.md
- [ ] Maintainer triggers `terraform plan` locally (dry-run, no apply)
- [ ] Maintainer confirms the 17 AWS infra lockdown tests pass
- [ ] Manual cross-check of the 2 DRIFT items against Dhan docs

## Rollout

1. Merge this PR into `main`
2. Run `make aws-bootstrap-check` on your local machine
3. Follow `docs/runbooks/aws-deploy.md` phases 1-4 (~50 min human)
4. Trigger the GitHub Actions `deploy-aws` workflow manually
5. Watch Telegram + Grafana for the first 30 minutes
6. Confirm the `sandbox_only_until = 2026-06-30` gate is active
7. No Live orders possible before 2026-06-30

---

**Session lineage for audit:** S1 foundations → S2 locked facts →
S3 backfill → S4 wiring audit → S5 honest audit → S6 mechanical
enforcement → S7 AWS + dep + doctor → S8 OIDC + Dhan verify →
S9 CVE + scanner fix + alarms + Makefile → S10 SDK addendum +
lockdown tests.

All plan files archived in `.claude/plans/archive/`.
