---
paths:
  - "crates/core/src/pipeline/**/*.rs"
  - "crates/trading/**/*.rs"
  - "crates/core/src/instrument/**/*.rs"
  - "crates/app/src/main.rs"
  - "config/**/*.toml"
---

# Market Hours & Trading Rules

## Key Facts
- Timezone: ALWAYS Asia/Kolkata (IST)
- Data collection window: 9:00-16:00 IST
- Trading window: 9:15-15:29 IST
- NEVER submit orders at or after 15:30 IST
- 375 candles/day (9:15-15:30, 1-minute)
- Pre/post market data stored separately

## Implementation Rules
- ALL timestamps include IST alongside any UTC
- Market hour validation uses config values, NOT hardcoded times
- Holiday calendar checked before every trading day

## Deep Reference
Read `docs/standards/market-hours.md` ONLY when implementing time-dependent logic or SEBI compliance.

## 2026-07-17 Update — deploy market-hours guard: red block → GREEN deferred no-op (main-branch push runs only)

The 2026-06-03 deploy market-hours lock (operator verbatim: *"only between 9 am
till 3.45 pm it should not happen"* — the `guard_market_hours` job in
`.github/workflows/deploy-aws.yml`, 09:00–15:45 IST = 03:30–10:15 UTC Mon–Fri;
until today the lock had no dedicated rule file — it lived in the workflow
comment + the ratchet `crates/common/tests/aws_infra_wiring.rs::test_deploy_aws_workflow_has_market_hours_guard`;
this note is now its rule-file home) changes FAILURE MODE, not substance:

- **The lock's substance is UNCHANGED:** nothing deploys inside the window
  without the explicit `workflow_dispatch` + `confirm_market_hours=yes`
  override. The deferred path deploys NOTHING (the Deploy-to-EC2 job is
  skipped via the guard's new `deploy_allowed` output).
- **What changed:** an in-window run for a BRANCH PUSH TO MAIN (a merge) now
  exits the guard GREEN as a deferred no-op (`deploy_allowed=false`
  + a `::notice::` naming the deferred SHA) instead of `exit 1` red. An
  in-window `workflow_dispatch` WITHOUT the override keeps the loud red
  `exit 1` — an explicit human/dispatcher action gets honest failure feedback.
- **Honest scope — the green defer covers MAIN-BRANCH PUSH runs only.** Three
  residual RED shapes remain by design: (i) an in-window catchup DISPATCH
  after a bot merge still exits 1 red (coverage still lands via the 15:46 IST
  after-close cron); (ii) the swap-time re-gate (`market_hours_swap_abort`)
  still reds a run whose build crosses 09:00 IST; (iii) in-window TAG pushes
  (`v*.*.*`) still block red — deliberately, BECAUSE the after-close cron
  compares/dispatches main HEAD only, so a green tag deferral (possibly on a
  non-HEAD commit) would be a silent never-deploy.
- **Authority:** the operator's zero-touch mandate (relayed via the
  coordinator session 2026-07-17). The failure it fixes: every market-hours
  merge produced a red push-triggered deploy run, and `postmerge-catchup.yml`
  counts ANY run (any color) as coverage for the SHA — so prod ran stale code
  until someone manually dispatched. The deferred green run is provably picked
  up by `deploy-aws-after-close.yml`'s 15:46 IST cron: its compare accepts as
  "last deployed" only runs whose B9 "Record deployed binary git SHA" step
  actually executed, and the skipped deploy job leaves that step `skipped`.
- **Division of authority (do NOT "fix" postmerge-catchup back):**
  `postmerge-catchup.yml` backfills RUN EXISTENCE only — its any-status probe
  is deliberate (it prevents 30-min re-dispatch storms and covers the
  out-of-window bot-merge delivery case). The DEPLOYMENT authority is
  `deploy-aws-after-close.yml`'s compare step: it walks the GitHub API's
  recent successful deploy-aws runs on main and accepts a run as the last
  REAL deploy only if its B9 "Record deployed binary git SHA" step (the one
  that stamps `/tickvault/prod/deploy/binary-git-sha`) has
  `conclusion == "success"` — a green deferred run (deploy job skipped, B9
  never executed) is ignored, so the 15:46 IST cron redeploys main HEAD. It
  is therefore immune to green-but-not-deployed runs, and a green deferred
  run counting as "covered" in catchup is correct, not masking. Independent
  ground-truth backstop: the deploy-watchdog Lambda's `binary_is_stale`
  check (handler.py) separately compares the SSM param
  `/tickvault/prod/deploy/binary-git-sha` — written only after a
  verified-healthy binary swap — against main HEAD on the box-start firing
  and publishes `tv_binary_main_sha_mismatch`, which the 24h
  `tv-<env>-binary-sha-stale` alarm pages on — so even a cron-miss on a
  deferred SHA surfaces within a day.
- **Immediate in-window delivery** remains available ONLY via the loud
  `workflow_dispatch` with `confirm_market_hours=yes` — kept by design.
