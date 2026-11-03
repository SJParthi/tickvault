## Summary

- Restore Stocks with All/F&O/index selection, Gainers/Losers, six server-side sorts, search before pagination and an identity-only watchlist. Unknown membership, unavailable value, closed markets and stale quotes stay explicit; portal/CSV functions remain.
- Integrate recovered features/dependencies and repairs to evening state, authentication, provider isolation, coverage/source guards, watchdog ancestry, spill/WAL visibility, conditional parked-ack release, Muhurat eligibility, private token files and recovery. Optimize the publication scanner without relaxing its deadline/baseline; add the official NSE CM/F&O January 15, 2026 closure.
- Apply the authorized November native $75 budget, preserving historical October $185 settings and the UTC clamp. Terraform/deployment refuse unreadable or malformed stop markers with bounded, byte-preserving reads. Deployment pins revisions, backs up artifacts and fixes Lambda digests. Depth20, Depth200 and live orders remain off.

## Type of Change

`feat`, `fix`, `refactor`, `test`, `docs`, `chore`, `perf`, `security`.

## Recovery / PR Disposition

One combined PR preserves original/unpublished work. `a9b0e89c` and its old-cloud-only bundle remain missing; Stocks is reconstructed. At **Nov 3 03:19 UTC**, all 24 original PRs remained open; main was `373282067`.

| Scope | Proposed disposition after accepted landing |
|---|---|
| #2079, #2144, #2149, #2158, #2170–#2176, #2178–#2181 | 15 integrated heads; #2161/#2162 also exact ancestors |
| #2152, #2159, #2164 | Reviewed continuations #2173, #2172, #2180 |
| #2184/#2186 | Paired AWS-LC 1.19.0/0.47.0 graph |
| #2169 | Exact original handoff archived as historical |
| #2185 | Held: upstream constraints conflict with duplicate-family policy; retain parking_lot 0.12.5 |

The 130 historical names/128 tips, including 21 formerly unresolved tips, have explicit dispositions; unshipped alternatives remain identified.

## Quality Gates

**Commit: `8b07b2973a8072dbbc0a6ae31336cfbd532504d6`.** Native commit passed in 10.087s. Touched-crate all-target Clippy passed in 264.959s on the exact committed diff. The scanner passed in 33.255s with 306; its baseline tightened from 307 to 306. Plan verification recorded 97 checked/22 open items. Typo checking was skipped: `typos` is unavailable.

Whole-workspace all-target Clippy passed on this exact clean commit in 198.610s. Native publication PASSED all11 local gates on this exact commit in1318.733s, completing Nov3 04:17:40UTC. Full workspace tests, formatting, Clippy, audit/deny, both Loom suites and source/count/integrity/financial guards passed. The branch is published. Exact-source remote CI/coverage/DHAT, infrastructure plan and deployment remain pending. Native log SHA256:194309f9c6d13b86e58c6c763b668188fe0942c86c0a2636baf8c7058263524c.

## Testing Done

| Evidence | Scope |
|---|---|
| Nov 3 03:41:34 UTC native run | **111 passed, 0 failed, 0 ignored** across seven scanner/calendar/filesystem guard targets, including four new scanner regressions. |
| Nov 3 03:06 UTC native run | **137 passed, 0 failed, 0 ignored** across budget/deployment/infrastructure/workflow/shell guards on the three-patch source. |
| Reader/timeout targets | 22/22 passed on source incorporated into `19231959`; warmed heap limits and actual retained ownership. |
| Browser | 28/28 Chromium checks plus timed validation/lock regression on `6226059a…`; synthetic responses. |
| Owned QuestDB | Five round-trip checks plus restart readback at `f521`; local database only. |

Counts are per run, not additive. Unit/integration, property, Loom, DHAT, boundaries, replay, financial and security checks apply. Fuzz campaigns/destructive production chaos were not run. Failed fixtures and attempts remain preserved.

## CLAUDE.md Compliance

Dependency pins, coverage floors and scanner baseline 306/120-second deadline remain. Static inventory 16,543 is not runtime coverage. Fresh coverage/delta, branch/MC/DC and Criterion/p99 are unmeasured. Filtering is O(n), sorting O(k log k), backup O(bytes); no universal O(1) claim.

## Operator Charter Compliance

| Requirement group | Evidence or remaining limit |
|---|---|
| Audit/monitoring/logging/alerting | Persistence/telemetry retained; typed failures reviewed; complete live delivery unverified. |
| Security/hardening | Independent reviews; operator key intentionally enters browser-memory authentication. Secret Scan pending. |
| Functionality/scenarios/review/extreme checks | Behavior tests, negative controls and ratchets; static counts are not coverage. |
| Performance/resilience/Telegram | Allocation checks retained; complete message-style/live-throughput proof unverified. |

## Zero-Loss Guarantee Charter

Parked-ack release follows the writeback/probe contract, not applied-row/fsync proof. Reader limits bound warmed incremental heap, not RSS/TLS. No exhaustive-coverage, uninterrupted-feed or zero-loss guarantee.

**Live baseline, Nov 3 03:11–03:16 UTC:** EC2 is running old build `373282067`. Selected depth/live-order metrics are zero; full boot-loaded configuration is partly unverified. Recovery changes are not deployed. The watchdog GitHub-token parameter is confirmed absent; ancestry logic does not provision it. The 2027 official calendar remains unverified; the 58-day horizon warning stays active.

- [ ] Confirm native publication, exact-source CI/coverage/DHAT and Secret Scan.
- [ ] Review real infrastructure plan, credential/configuration prerequisites and permitted deployment window.
- [ ] Verify app/helper/portal identity, Stocks, budget read-back and activation settings.
- [ ] Recheck original PR heads/landed content; reconcile covered PRs and retain unresolved #2185.


## Claude Code handoff — owner requested continuation from GitHub

The owner requested that recovered Codex work and pending PR changes be placed in **one draft PR**, with remaining review, merge and deployment continued from Claude Code. Treat this PR branch as the combined source of truth. Preserve original PR branches and unpublished history; do not reset, rebase or delete them. The branch contains 295 changed files versus the recovered main, with 62,299 additions and 3,110 deletions.

### Start here

1. Fetch and inspect `fix/tickvault-integrated-recovery-20261102`; expected head is `8b07b2973a8072dbbc0a6ae31336cfbd532504d6`, tree `28659144efec92b406d8a63f762ef8f478290264`. Read `CLAUDE.md`, applicable `.claude/rules/`, and `.claude/plans/active-plan-audit-2026-10-20.md`. The plan remains IN_PROGRESS: 97 checked items and 22 open, not a blanket completion claim.
2. Keep **Depth20, Depth200 and live-order activation OFF**. Retain depth sockets disabled, strategy dry-run, sandbox mode/cutoff2099-12-31 and exit orders disabled. Do not use market-hours overrides, reset budget/holiday stop markers, rotate the operator key, or restart capture merely to validate the UI.
3. Use this one PR for further corrections. Re-read its actual head, main and each original PR before merging or reconciling anything. The 24-PR table above is a proposed resolution, not evidence that those PRs were individually merged. #2185 parking_lot0.13 is incompatible with the current Tokio/DHAT/governor/DashMap graph under the duplicate-family policy; retain0.12.5 unless a compatible upstream graph is proven.

### Completed source and local verification

The source includes the reconstructed Dhan-style Stocks view, F&O/index membership filters, gainers/losers, sorting/search/pagination/watchlist, authenticated Rust movers path, recovery/state-integrity fixes, strict integer parsing, proof/coverage corrections, storage/WAL visibility and release safeguards. The original missing `a9b0e89c` was not recovered; do not describe reconstructed Stocks as that exact commit.

The final public-function hook retains the original matching semantics and120s deadline. It uses an optional private text index, falling back for binary/invalid encoding/unusual paths/errors. Final hook SHA256`e12b5307c259e31b97eb2f4547b967fd459b52022016e1ee15e30530c262b674`; regression source SHA256`5e50c1e59a4a495444d2f7e22bb2b50995be334a2a5feb4c84d21e91184500cb`. The actual combined scan took33.255s/count306. A real401-file213,229-byte `/tmp` mutation control passes original/final and rejects brokenV3. Baselines are306 untested and16,543 static test attributes. Earlier failed optimizations and fixture failures were preserved, not represented as passes.

The scoped111-test run passed at03:41:34UTC (log SHA256`cf8bb68720eecac6d23e3bb07e7b453d22ef30acd9719374c98f214dab7b6528`). The separate137-test budget/fence run passed at03:06UTC (log SHA256`b4f377cf76e4b04b44ea170c5e3438b5632577c21d5f6fde675d46e7f0280a16`). Full all-target workspace Clippy on clean8b07 passed198.610s (log SHA256`26d0b9e381e1ff3ae388e94ea3cbc7ce325c1ddba785374fd0622e905de2ae1a`). Native commit checks passed; the unavailable typo tool was explicitly skipped. Counts are per run and are not coverage percentages.

### Required remaining checks and merge

- Require exact-final-source `All Green` and Secret Scan. Inspect all dependent jobs, dedicated DHAT and actual `coverage-report` artifact; artifact existence alone is not a pass. Preserve configured coverage floors in `quality/crate-coverage-thresholds.toml` and run the actual `scripts/coverage-gate.sh` on the downloaded report. CI uses Rust1.95.0/cargo-llvm-cov0.9.1; default coverage skips `dhat_` tests, so dedicated allocation jobs are separate evidence. Fuzz, mutation and sanitizers are scheduled lanes; do not claim they ran here.
- A real Terraform plan is still needed: dispatch `terraform-apply.yml` on the frozen feature branch with `skip_apply=yes`, `confirm_market_hours=no`, and empty `force_unlock_lock_id`/`rotate_console_key`. Its PR lane only does offline validation. A daytime plan-only dispatch may finish the plan then fail its separate market-hours guard; report the actual job outcomes, not overall success. Never expose raw plan/state secrets.
- Inspect actual plan actions: native November budget75USD (October185 exception expired), no EC2/EBS replacement or data deletion, no held daily-start enablement, intended scheduler PassRole narrowing and Lambda package changes. The fail-closed marker reads distinguish actual ParameterNotFound from denied/unavailable/malformed responses. Apply replans, so review applied drift too.
- Follow `docs/claude-rules-full/project/merge-gate-lock-2026-07-04.md`, especially exact-head All Green and the repository bot merge path. Do not agent-arm auto-merge or use admin bypasses. After landing, verify the accepted tree and fresh original PR heads. Resolve the23 covered PRs as incorporated/superseded where appropriate; do not call a closed or squash-superseded PR individually merged. Preserve their branches; hold#2185.

### Live AWS findings and deployment

The latest observed instance baseline on Nov3 is **RUNNING**, not yesterday's stopped state. At09:31IST, health was good on OLD fullSHA`37328206793e099b0781c4b449a0859e17387a73`, stablePID3257/start_ticks693, with five main-feed connections. About3.1s of observation showed+8,938 ingest updates and+5,258 buffered quote appends. Depth20/200/depth-enabled and live-order counters werezero. Drop/spill counters both7,412 were unchanged in that interval, consistent with spill rescue but not verified replay/durability.593 unwritten seals and one35,616,998-byte WAL segment remained. This is not database-commit or zero-loss proof. No general reconnect count was verified.

The misleading08:50 stale-build alert used SSM deployment mirror`49dd25239bde21411008b11bd76c51d5e0f8284f`, two workflow-only commits behind main, because GitHub-token retrieval failed. Exact-name SSM metadata at03:30:21UTC confirmed `/tickvault/prod/operator/github-token` absent; source IAM/path wiring matches. The ancestry fix is included, but it does not provision the missing credential. Do not substitute guessed credentials or broaden IAM without evidence. Provision any required credential through secure AWS configuration, not PR text or logs.

The holiday warning is correct:58 days remained to31Dec2026 under a60-day warning threshold. Jan15,2026 was separately missing and is now corrected from official [CMTR72260](https://nsearchives.nseindia.com/content/circulars/CMTR72260.pdf) and [FAOP72262](https://nsearchives.nseindia.com/content/circulars/FAOP72262.pdf). No authoritative2027 circular was verified; do not invent future dates or suppress the reminder.

After required gates and merge, deploy BOTH the EC2 app/helper and portal Lambda through the existing guarded workflows. The09:00–15:45IST capture window is protected. Preserve the actual current instance baseline and private pre-deploy backups; never stop today's running server just because yesterday's record said stopped. Check bot-merge push-event suppression and existing postmerge/after-close catch-up workflows rather than assuming deployment occurred.

Verify full running app SHA, helper artifact, LambdaCodeSha256/PORTAL_GIT_SHA and actual authenticated Stocks behavior. Public portal URL: https://djxhgi8j60.execute-api.ap-south-1.amazonaws.com/ . The current source HTML template SHA256 is`6226059a333a70a726bf4a34da7d4dc5c57818efd95951244bea42efab4d88da`; served HTML adds a footer. A short footer SHA or HTTP200 typed error is not full deployment proof. Test All/F&O/index membership, gainers/losers, all six sorts, search before pagination, watchlist persistence without cached prices, expiry/auth failures and mobile layout. Synthetic28+1 checks passed previously; live new-build validation remains pending. After-close empty rows cannot prove market-open ranking or latency.

### Performance, media and preservation limits

No universal O(1), submicrosecond p99, zero-bug/zero-loss or exhaustive-coverage guarantee is established. Historical ec997 four-update frame p99 measurements5.620/10.448/9.785us failed the local4us frame budget and strict1us diagnostic; they are not final8b07 measurements. Actual `bench.yml` timing is outside All Green and supports manual branch dispatch with `reset_baseline=false`; verify exact run/checkout SHA. Its inherited CI multiplier2.5 gives a10us legacy frame ceiling, distinct from the strict1us diagnostic. Shared benchmark queue/cache effects and an existing missing-Criterion-directory skip need explicit interpretation; no real CI exposure of that optional/manual weakness was demonstrated.

A103.333s narrated Part1 video and100-name shortlist were generated locally; final live-deployment video chapters remain unfinished. No product URL was selected or domain availability verified. These media files and raw local audit reports are not represented as repository source by this PR description.

All original local refs and evidence were preserved. Final local full all-ref bundle:70,920,631bytes,957 advertised entries, SHA256`b882333ca214c8e0927f70550088c0ac5079d605cff54ef6a4042d56c81754aa`, verified with git bundle verify. The cloud workspace evidence root is `/workspace/tickvault-recovery/live-assurance-20261102T071741Z`; startup status is `/workspace/tickvault-setup/CURRENT-STATUS.md`. Those local paths are not guaranteed to exist in Claude Code. This description therefore carries the essential handoff; fresh GitHub/CI/live verification must supply remaining proof.


GitHub reported four Dependabot alerts on the current default branch during push (two moderate, two low). The consolidated branch passed local cargo audit/deny; recheck the current repository alerts and exact-source CI before closing dependency work.
