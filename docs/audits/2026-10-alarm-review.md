# CloudWatch alarm review — 2026-10-22 (plan item 65, operator Quote 37)

Authority: `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`
§0 Quote 37 ("remove alarms that watch switched-off parts, remove metrics for
removed features … fewer alarms overall 127 → about 60"; dashboard KEPT).
Paging-family removals are recorded first in
`docs/claude-rules-full/project/dhan-rest-only-noise-lock-2026-07-14.md` §2.14.

## Counts (from the terraform source; no AWS read was possible from this session)

| | Count | How it was counted |
|---|---|---|
| Alarm resources in `deploy/aws/terraform/*.tf` | 104 blocks | `grep -rn 'resource "aws_cloudwatch_metric_alarm"\|aws_cloudwatch_composite_alarm' deploy/aws/terraform`; no composite alarms |
| …expanded for production | **125** | `error_code` `for_each` = 23 entries (+22); `dhan_depth_token_minter_not_invoked` `count = 0` while the depth account is off (−1); the QuestDB console (2) and operator control (1) alarms are on (`TF_VAR_enable_*` = true in `terraform-apply.yml`) |
| Live account, measured 2026-10-13 | 127 | the two extra are the Groww token-minter alarms built outside this repo (Quote 30 measurement) |
| **After this change, terraform, production** | **120** | 5 removed, 1 gated off |
| **After this change, live (Assumed)** | **122** | until the two outside-repo Groww alarms are deleted by hand (−2 → 120) |

**Verdict: NOT at ~60.** This change removes only what is switched off or
watches nothing. Getting near 60 means merging or removing alarms that still
watch live paths; those are listed under "Owner decisions" below with what
each merge would cost. None is taken here, because each touches a page the
zero-loss charter or the noise lock relies on.

## Removed (5) and gated (1)

| Alarm | What it watched | Disposition | What covers that failure now |
|---|---|---|---|
| `depth-steering-stalled` | depth steering heartbeat age (`tv_depth_rebalance_age_secs`, breaching, market-hours gated) | **Removed** (depth off 2026-10-21; the app published a fake 0 only to keep it quiet); also left the gate's `ALARM_NAMES` | not needed while depth is off |
| `depth-rescue-abandoned` | depth rescue rows abandoned at shutdown | **Removed** (no depth rows written) | `tick-rescue-abandoned`, `offload-writer-shutdown-incomplete`, `wal-spill-shutdown-incomplete` |
| `dhan-depth-new-contract-blank` | new depth subscriptions silent after 2 min (ratio) | **Removed** (no depth subscriptions) | not needed while depth is off |
| `errcode-ws-gap-02-swap-emptied-socket` | a depth swap that left a socket empty | **Removed** (the swap runs only on depth sockets) | not needed while depth is off |
| `order-fill-lag-high` | `tv_order_fill_lag_seconds` — no emit site, `actions_enabled = false` | **Removed** (watched nothing, paged nobody) | `orders-rejected`, `order-path-dropped`, `order-audit-chain-loss` |
| `dhan-depth-token-minter-errors` | errors of the depth-account minter Lambda | **Gated** on `dhan_depth_account_enabled` (off), like its schedule and not-invoked alarm | returns with the flag |

Metrics removed from the EMF selector with them (only reader was a removed
alarm, −$1.20/mo): `tv_depth_rebalance_age_secs`, `tv_depth_rescue_abandoned_total`,
`tv_depth_first_packet_arrived_total`, `tv_depth_first_packet_silent_total`.
Selector 113 → 109 names.

**Removed with item 62 (not alarms):** the four top_volume filter rows in the
`market-data-refused` loss group, `tv_spill_free_probe_blind_total{tier="top_volume"}`
and `Volume01MonotonicityBreach` (an error code, not an alarm resource). 3 s / 5 s
candles have no alarm of their own. Item 62's commit owns those.

## Every alarm (125 before, one row each)

K = kept, R = removed, G = gated off. "Why kept" names the failure class the
zero-loss charter or the noise lock needs paged.

### EC2 and host (`alarms.tf`, `app-alarms.tf`)

| Alarm | Watches | | Why kept |
|---|---|---|---|
| instance-status-failed | EC2 instance status check | K | box unreachable |
| system-status-failed | EC2 system status check | K | box unreachable (host fault) |
| cpu-high-5min | CPU > 90% for 5 min | K | capacity; candidate (see decisions) |
| ebs-write-latency-high | EBS write latency > 50 ms | K | disk slow → drain backs up |
| network-out-runaway | egress > 50 MB/s | K | cost runaway |
| questdb-disconnected | QuestDB down > 30 s | K | database down |
| token-remaining-low | Dhan JWT < 4 h left | K | noise-lock family (4) |
| orders-rejected | order rejected | K | order side |
| clock-skew-high | clock skew > 1 s | K | timestamps wrong |
| disk-used-high | root > 90% | K | disk filling |
| disk-fill-rate-high | fill rate trend | K | disk filling |
| disk-watcher-respawn | disk watcher flapping | K | disk monitor blind |
| mem-used-high | memory > 80% | K | capacity, more relevant on r8g.large |
| spill-dir-free-low | ≤ 20 GiB free | K | disk filling (noise-lock §2.3g) |
| questdb-wal-suspended | tables WAL-suspended | K | silent row loss (§2.3g) |
| questdb-wal-apply-lag | WAL apply falling behind | K | data not queryable |
| questdb-wal-probe-failed | suspension probe failing | K | monitor blind; overlaps `wal-suspension-probe-blind` (see decisions) |
| api-auth-failed | 401 burst on public API | K | security |

### Live lane (`live-lane-alarms.tf`, family 5)

| Alarm | Watches | | Why kept |
|---|---|---|---|
| dhan-live-lane-down | lane-up gauge | K | feed down |
| dhan-socket-parked | socket parked for good | K | feed coverage lost |
| ticks-dropped | ticks dropped minus spilled | K | ticks lost |
| ws-ring-full | frame ring slots full | K | frames refused |
| ws-ring-bytes-full | frame ring bytes full | K | frames refused (merge candidate with the above) |
| live-universe-fallback | universe fell back | K | coverage lost |
| ws-no-alive-connections | every socket gone | K | feed down |
| wal-frames-not-recovered | replay could not recover frames | K | frames lost |
| dhan-wal-dropped | frame missed the WAL | K | ticks lost (no second signal) |
| ticks-lost-spill | spill writer shed a frame | K | ticks lost |
| dhan-no-ticks-flowing | no tick persisted ~10 min | K | feed down |
| dhan-contract-universe-failed | contract universe failed | K | coverage lost |
| tick-spill-replay-failing | rescued ticks not replayed | K | rows held back |
| ticks-spilling | tick flush rescued to disk | K | early warning; candidate (no loss yet) |
| depth-steering-stalled | depth steering heartbeat | **R** | — |
| dhan-worst-socket-deaf | one socket silent 10 min | K | feed coverage lost |
| wal-replay-unknown-magic | unreadable WAL format | K | frames lost |
| seal-writer-rescued | candles escalated to disk | K | early warning; candidate (no loss yet) |
| aggregator-slots-exhausted | candle slots full | K | candles lost |
| tick-spill-quarantined | rescue file refused 4xx | K | rows set aside |
| wal-catchup-budget-exhausted | catch-up left segments | K | rows held back |
| offload-writer-shutdown-incomplete | writer cut off at exit | K | rows lost at shutdown |
| tick-rescue-abandoned | tick rescue cut off at exit | K | rows lost at shutdown |
| depth-rescue-abandoned | depth rescue cut off at exit | **R** | — |
| wal-spill-shutdown-incomplete | WAL writer cut off at exit | K | frames lost at shutdown |
| wal-replay-restore-failed | deferred segment not restored | K | frames stranded |
| aggregator-refusal-rate-high | > 25% ticks refused a bucket | K | candle quality |
| dhan-ring-dwell-high | frames waiting in the ring | K | early warning of ring full |
| dhan-feed-delay-high | trades ≥ 60 s late | K | §2.6 delay page |
| dhan-main-reconnect-slow | main-feed reconnect ≥ 15 s | K | §2.6 delay page |
| dhan-depth-new-contract-blank | new depth contracts blank | **R** | — |

### Loss groups, data at risk, loss and retention

| Alarm | Watches | | Why kept |
|---|---|---|---|
| market-data-refused | 9+ counters, market data refused | K | §2.10 loss group |
| subscription-coverage-lost | planned contracts with no data | K | §2.10 |
| audit-rows-lost | audit rows lost | K | SEBI/audit loss |
| durability-sync-failing | disk sync failing | K | §2.10 |
| order-path-dropped | order updates/fills dropped | K | §2.10 |
| safety-bound-or-monitor-blind | table full or monitor blind | K | §2.10 |
| cold-backup-failing | uploads to S3 failing | K | §2.9 data at risk |
| disk-kept-not-backed-up | prune refused for no copy | K | §2.9 |
| order-update-dropped | order update reached nothing | K | §2.9 |
| log-lines-dropped | log lines dropped | K | §2.9 |
| feed-thread-wrote-to-disk | drain wrote inline | K | §2.9 |
| partition-archive-failed | retention not freeing disk | K | disk filling |
| market-data-persistence-loss | tick/depth rows not in QuestDB | K | ticks lost |
| durable-floor-breach | WAL write failed | K | ticks lost |
| wal-suspension-probe-blind | suspension probe blind for polls | K | monitor blind (overlap, see decisions) |
| seal-writer-dropped | sealed candles truly dropped | K | candles lost (duplicate page with errcode aggregator-drop-01, see decisions) |
| telegram-drops | Telegram pages dropped | K | the pager itself |

### Order side (`order-side-alarms.tf`)

| Alarm | Watches | | Why kept |
|---|---|---|---|
| orders-placed-storm | 50+ orders in 5 min | K | runaway strategy |
| daily-loss-breach | daily P&L below limit | K | risk halt |
| order-fill-lag-high | fill lag (no emitter) | **R** | — |
| order-audit-chain-loss | SEBI order audit rows lost | K | SEBI/audit loss |

### Coded-error log filters (`error-code-alarms.tf`, 23 → 22, plus 3 standalone)

| Alarm | Watches | | Why kept |
|---|---|---|---|
| errcode-dh-901 | Dhan token rejected | K | token failing (family 3) |
| errcode-dh-906 | order error | K | order side |
| errcode-auth-gap-04 | boot mint failed | K | token failing |
| errcode-proc-01 | process failure | K | app down |
| errcode-resource-02 | resource exhaustion | K | capacity |
| errcode-aggregator-drop-01 | candles dropped | K | candles lost |
| errcode-aggregator-stall-01 | drain waited on disk | K | §2.7 |
| errcode-hot-path-stall-01 | runtime or drain stalled 2 s | K | §2.8 |
| errcode-wal-suspend-01 | WAL suspension | K | silent row loss |
| errcode-auth-gap-05-remint-failed | forced re-mint failed | K | token failing |
| errcode-boot-02 | QuestDB boot probe deadline | K | boot blocked |
| errcode-boot-03 | clock skew at boot | K | timestamps wrong |
| errcode-oms-gap-06 | paper order runtime died | K | order side |
| errcode-ws-spill-01 | WAL disk refused write | K | ticks lost |
| errcode-ws-spill-02 | frame dropped at the WAL | K | ticks lost |
| errcode-storage-gap-05 | pressure archival gave up | K | disk filling |
| errcode-risk-gap-03 | connected but hearing nothing | K | feed silent |
| errcode-ws-gap-03-universe-collapse | universe collapsed | K | coverage lost |
| errcode-ws-gap-03-xverify-vacuous | cross-check compared nothing | K | §2.5 |
| errcode-ws-gap-03-xverify-failed | cross-check failed | K | §2.5 |
| errcode-ws-gap-03-xverify-diverged | cross-check found mass divergence | K | §2.5 |
| errcode-ws-gap-02-swap-emptied-socket | depth swap emptied a socket | **R** | — |
| errcode-scoreboard-01 | daily rollup failed | K | audit view lost |
| preopen-ready-late | lane attach after 09:12 | K | feed late |
| errcode-hot-path-02 | persistence lost rows | K | ticks lost |
| errcode-stream-silent | the error log stream stopped | K | the pager's input blind |

### Lambdas and control plane

| Alarm | Watches | | Why kept |
|---|---|---|---|
| dhan-token-minter-errors | daily mint failed | K | token failing |
| dhan-token-minter-not-invoked | mint did not run | K | token failing |
| dhan-depth-token-minter-errors | depth-account mint failed | **G** | — |
| dhan-depth-token-minter-not-invoked | depth-account mint did not run | already `count = 0` | (not counted in 125) |
| start-watchdog-errors / -not-invoked | 08:30 start Lambda | K, K | box not started |
| hard-stop-guard-errors / -not-invoked | hourly budget stop Lambda | K, K | spend stop |
| daily-budget-digest-errors / -not-invoked | spend digest | K, K | spend visibility |
| budget-killswitch-errors | 100% budget killswitch | K | spend stop |
| deploy-watchdog-errors / -not-invoked | stale-binary check | K, K | wrong binary running |
| binary-sha-stale | binary differs from main 24 h | K | wrong binary running |
| deploy-provenance-blind | watchdog could not compare | K | monitor blind |
| market-hours-gate-errors / -not-invoked | the gate that arms in-session alarms | K, K | the gate itself |
| market-hours-liveness-missing | app liveness absent in session (~5 min) | K | app down (faster than no-ticks) |
| boot-heartbeat-missing | boot heartbeat absent | K | app did not boot |
| boot-heartbeat-gate-errors / -not-invoked | the boot gate | K, K | the gate itself |
| market-open-readiness-errors / -not-invoked | 08:45 readiness pager | K, K | the pager itself |
| eventbridge-dlq-depth | start/stop invocation dead-lettered | K | box not started/stopped |
| telegram-webhook-errors | Telegram webhook Lambda | K | operator commands |
| operator-control-errors | console Lambda | K | operator tooling (candidate) |
| questdb-console-front-errors / -proxy-errors | QuestDB console Lambdas | K, K | operator tooling (candidate) |
| logs-ingestion-runaway | > 5 GB logs a day | K | cost runaway |
| app-log-ingestion-silent | app log shipping stopped | K | every coded page blind |

## Owner decisions (not taken; each needs its own review row and, for a §2 page, a dated noise-lock row)

| Candidate | Alarms saved | Cost of doing it |
|---|---|---|
| Delete the two Groww token-minter alarms built outside this repo (Groww removed 2026-08-21) | 2 | none known; needs a console or CLI delete with AWS access |
| One metric-math alarm over the Lambda `Errors` metrics (13 live alarms; a metric-math alarm takes at most 10 metrics, so 2 alarms) | about 11 | while one Lambda is in ALARM a second failing Lambda adds no new page; the page no longer names the Lambda; breaks `every_lambda_has_an_errors_alarm_or_a_declared_exemption` |
| One metric-math alarm over the 8 "did it run?" `Invocations` alarms | about 6 | different periods (6 h / 24 h / 3 d) and some are gated, some not; would need splitting by period |
| Merge `ws-ring-full` + `ws-ring-bytes-full` | 1 | one page for two different limits |
| Merge the three shutdown-cut-off alarms (`tick-rescue-abandoned`, `offload-writer-shutdown-incomplete`, `wal-spill-shutdown-incomplete`) | 2 | page no longer names which writer |
| Merge `wal-replay-unknown-magic` + `wal-replay-restore-failed` + `wal-frames-not-recovered` | 2 | same |
| Keep one of `questdb-wal-probe-failed` / `wal-suspension-probe-blind` | 1 | first-failure page vs several-polls page |
| Keep one of `seal-writer-dropped` / `errcode-aggregator-drop-01` (same event, two pages) | 1 | lose the counter route or the log route; the log route is the fallback when metrics shipping breaks |
| Drop the "no loss yet" early warnings `ticks-spilling`, `seal-writer-rescued` | 2 | the loss pages still fire if the rescue then fails; lose the early warning |
| Drop operator-tooling Lambda alarms (`operator-control-errors`, two console alarms) | 3 | a broken console is found at use time instead |
| Drop paper-mode order alarms (`orders-placed-storm`, `orders-rejected`, `errcode-dh-906`, `errcode-oms-gap-06`) until live orders | 4 | must be restored before any live order |
| Drop `cpu-high-5min`, `network-out-runaway` | 2 | lose a capacity and a cost signal |

Taking every row: 122 → about 85 live. Reaching about 60 would also need
merging loss pages the zero-loss charter names, which this review does not
recommend without the owner.

## Rollback

Re-apply terraform from the commit before this change; restore the four
selector names in `deploy/aws/cloudwatch-agent.json`.
