# CloudWatch alarm and metric review — 2026-11-11 (operator Quote 53)

**Authority:** operator Quote 53 (2026-11-11), recorded in
`docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`.
The owner ticked "Cut alarms 93 → ~30 (Recommended)", "Trim CloudWatch metrics
(Recommended)", "Cheaper S3 class for old raw frames (Recommended)" and "Delete
the CloudWatch dashboard", and chose "No, keep $75 stop for now (Recommended)".
Quote 53 is headed "FROM DECEMBER": apply this change on or after 1 December
2026 (or earlier only with the owner's go-ahead).

**KEPT list (binding, Quote 53):** every data-loss and loss-group page, the
lane-down / socket-parked / ticks-dropped live-lane pages, the token pages, the
budget pages, the disk-full and QuestDB-down pages, the cross-verification
pages. Every REMOVE row below says what still covers the failure.

## Result

| | Before | After |
|---|---|---|
| Alarm resources in terraform (prod, count-gated depth minter alarms excluded) | 119 | 82 |
| Coded-error (`errcode-*`) map entries | 22 | 18 |
| EMF metric names shipped to CloudWatch | 109 + 1 class series | 46 |
| CloudWatch agent host metrics (disk, memory, CPU) | 3 | 1 (disk) |
| Metric filters removed with their alarm | — | 1 (`preopen_ready`) |
| CloudWatch dashboard | 1 | 0 |

**Honest gap against the owner's "~30":** 82 alarms remain. Reaching about 30
would mean removing alarms on the KEPT list (for example the twelve separate
data-loss pages in `live-lane-alarms.tf`), which Quote 53 forbids. The second
table at the end lists the alarms kept only by a broad reading of "data loss"
and "live lane"; the owner can approve removing any of them with a fresh dated
quote.

## Corrections after the first draft's review (2026-11-11)

The first draft removed 44 alarms. Its review found five of them were the only
page for a failure on the KEPT list, and one stated date was not enforced. A
second review found two more (the boot-heartbeat gate), one wrong cover, and
that the date gate did not cover the metric half:

| Item | Finding | Now |
|---|---|---|
| `errcode-stream-silent` (+ its two schema filters) | errors.jsonl and app.log ship to the same log group, so `app-log-ingestion-silent` cannot see errors.jsonl alone stop; every KEPT `$.code` page would go dark with no page (noise lock §2.3s REJECTs this exact removal) | KEPT |
| `live-universe-fallback` | Also counts `truncated_to_capacity`, `artifact_unreadable`, `artifact_unparseable`, `no_usable_widening`; the universe-collapse filter matches only `fell_back_to_indices` | KEPT |
| `tick-spill-quarantined` | A whole quarantined tick file is missing from QuestDB; no other page counts whole files | KEPT |
| `wal-suspension-probe-blind` | The only blind detector left for the kept `questdb-wal-suspended` | KEPT (`questdb-wal-probe-failed` and the WAL-SUSPEND-01 filter stay removed) |
| `deploy-provenance-blind` | Without it the kept `binary-sha-stale` reads OK when it does not know | KEPT |
| `disk-used-high`, `spill-dir-free-low` descriptions | Pointed at the removed `disk-fill-rate-high` as the early warning | Reworded: no trend alarm warns days ahead |
| "From December" | `terraform-apply.yml` applies on every push to main, so a November merge applied in November | `terraform_data.quote53_not_before_december` in `main.tf` refuses any plan before 2026-12-01 00:00 IST unless `quote53_early_apply_approved` is set true with the owner's dated go-ahead. Risk: while it holds, every terraform apply fails, not only this one; merge on or after 1 December. |
| "From December", the metric half (second review) | `deploy-aws.yml` copies `deploy/aws/cloudwatch-agent.json` to the box on every merge that touches it, and every morning boot copies the repo copy, with no terraform in either path. A November merge would have stopped shipping the trimmed metrics in November while the terraform gate kept every alarm that reads them, so about 18 of them would sit green on missing data all month (`notBreaching`), the deaf-socket page among them | Test 9 of the guard fails the build while the trimmed agent config is in the tree before 2026-12-01 00:00 IST, unless the same `quote53_early_apply_approved` switch is set true. So this change cannot merge before 1 December (All Green), and a November merge cannot apply either half. |
| `boot-heartbeat-gate-errors`, `boot-heartbeat-gate-not-invoked` (second review) | The kept `boot-heartbeat-missing` ships disarmed and only this gate arms it; with both removed, a failing or dropped gate left it off unseen, and the first page was the liveness page after 09:20 IST | KEPT (both back in terraform, the gate list and the Muhurat skip list) |
| `wal-replay-restore-failed` cover (second review) | The row said "nothing deleted"; a segment that cannot be moved back is archived unread, a data-loss path | Still removed: its page is the coded `WS-SPILL-02` ERROR at the same site, matched by the kept `errcode-ws-spill-02`. Test 10 pins that link. |

Pinned by `crates/common/tests/quote53_alarm_review_cover_guard.rs`.

Re-count with:

```
grep -c '^resource "aws_cloudwatch_metric_alarm"' deploy/aws/terraform/*.tf
```

## Every alarm, one row each

Coded-error alarms (`errcode-*`) come from the `error_code_alerts` map in
`error-code-alarms.tf`; removing a map entry removes its metric filter and its
alarm together.

| Alarm | File | What it watches | Decision | Plain reason |
|---|---|---|---|---|
| `tv-<env>-instance-status-failed` | alarms.tf | EC2 instance status check | KEEP | Carries the EC2 `reboot` action that restarts a hung instance; not just a page. |
| `tv-<env>-system-status-failed` | alarms.tf | EC2 system (host) status check | KEEP | Carries the EC2 `recover` action that moves the box off failed hardware; not just a page. |
| `tv-<env>-cpu-high-5min` | alarms.tf | CPU above 90% for 5 min | REMOVE | Capacity signal, not loss. A stalled hot path is still paged by `tv-<env>-errcode-hot-path-stall-01`; refused frames by the ring-full pages. |
| `tv-<env>-ebs-write-latency-high` | alarms.tf | Disk write latency above 50 ms | REMOVE | Performance signal, not loss. A disk that stalls the feed still shows as `tv-<env>-errcode-hot-path-stall-01` and the ring-full pages. |
| `tv-<env>-network-out-runaway` | alarms.tf | Outbound network bytes | REMOVE | Cost signal; the $75 budget stop (Quote 37) bounds any runaway, and the budget pages are kept. |
| `tv-<env>-questdb-disconnected` | app-alarms.tf | QuestDB unreachable | KEEP | QuestDB-down page (KEPT list). |
| `tv-<env>-token-remaining-low` | app-alarms.tf | Dhan token under 4 h left | KEEP | Token page (KEPT list). |
| `tv-<env>-orders-rejected` | app-alarms.tf | Order rejections | REMOVE | No real orders are placed (paper mode, `dry_run`). The `order-path-dropped` loss group stays. |
| `tv-<env>-clock-skew-high` | app-alarms.tf | Clock skew above 1 s | REMOVE | Boot refuses to start on skew above 2 s (BOOT-03), which `tv-<env>-boot-heartbeat-missing` and the liveness page then catch. |
| `tv-<env>-disk-used-high` | app-alarms.tf | Root disk above 90% | KEEP | Disk-full page (KEPT list). |
| `tv-<env>-disk-fill-rate-high` | app-alarms.tf | Disk filling faster than 4 points a day | REMOVE | Early warning only; the disk-full pages (`disk-used-high`, `spill-dir-free-low`, `errcode-storage-gap-05`) stay.  The descriptions of `disk-used-high` and `spill-dir-free-low` now say no trend alarm warns days ahead. |
| `tv-<env>-disk-watcher-respawn` | app-alarms.tf | Disk watcher task restarting | REMOVE | Supervised respawn, nothing lost; the disk-full pages stay. |
| `tv-<env>-mem-used-high` | app-alarms.tf | Host memory above 80% | REMOVE | Capacity signal; an out-of-memory kill is still paged by `tv-<env>-errcode-proc-01`. Its host metric is no longer shipped. |
| `tv-<env>-spill-dir-free-low` | app-alarms.tf | Free disk at or under 20 GiB | KEEP | Disk-full page (KEPT list). |
| `tv-<env>-questdb-wal-suspended` | app-alarms.tf | QuestDB tables not applying their WAL | KEEP | QuestDB-down page (KEPT list). |
| `tv-<env>-questdb-wal-apply-lag` | app-alarms.tf | QuestDB apply backlog | REMOVE | Lag, not loss; rows catch up on their own. A suspended table is still paged by `questdb-wal-suspended`. |
| `tv-<env>-questdb-wal-probe-failed` | app-alarms.tf | The WAL probe itself failing | REMOVE | A probe that keeps failing makes the gauge read -1, which `wal-suspension-probe-blind` (kept) pages; a QuestDB that is down is paged by `questdb-disconnected`. |
| `tv-<env>-api-auth-failed` | auth-failed-alarm.tf | 25+ bad API passwords in 5 min | KEEP | Not in the KEPT list; kept as the only page for a password-guessing sweep on the operator API (security). Candidate for the next round. |
| `tv-<env>-boot-heartbeat-missing` | boot-heartbeat-alarm.tf | App did not finish booting by the morning | KEEP | Live-lane page: the app is not up before the open. |
| `tv-<env>-boot-heartbeat-gate-errors` | boot-heartbeat-alarm.tf | The boot-heartbeat gate Lambda failing | KEEP | The kept `boot-heartbeat-missing` page ships disarmed and only this gate arms it. Without this alarm a failing gate leaves that page off with nothing to say so, and the first page is `market-hours-liveness-missing` after 09:20 IST, too late for the pre-open. (Removed in the first draft; put back after its second review, the `deploy-provenance-blind` reasoning applied to a live-lane page.) |
| `tv-<env>-boot-heartbeat-gate-not-invoked` | boot-heartbeat-alarm.tf | The boot-heartbeat gate not running | KEEP | Same as the row above: a dropped gate schedule produces no error, so only this alarm sees it. Stays in the market-hours gate list and in the Muhurat open skip list. |
| `tv-<env>-hard-stop-guard-errors` | budget-guards.tf | The hourly hard-stop guard failing | KEEP | Budget page (KEPT list). |
| `tv-<env>-daily-budget-digest-errors` | budget-guards.tf | The daily spend digest failing | KEEP | Budget page (KEPT list). |
| `tv-<env>-hard-stop-guard-not-invoked` | budget-guards.tf | The hard-stop guard not running | KEEP | Budget page (KEPT list). |
| `tv-<env>-daily-budget-digest-not-invoked` | budget-guards.tf | The daily spend digest not running | KEEP | Budget page (KEPT list). |
| `tv-<env>-budget-killswitch-errors` | budget-killswitch-lambda.tf | The budget kill-switch failing | KEEP | Budget page (KEPT list). |
| `tv-<env>-cold-backup-failing` | data-at-risk-alarms.tf | Raw frames not reaching S3 | KEEP | Data-loss family: the only off-box copy is failing. |
| `tv-<env>-disk-kept-not-backed-up` | data-at-risk-alarms.tf | Old files kept because they have no S3 copy | REMOVE | Nothing is lost (files are kept); the cause is paged by `cold-backup-failing`, the effect by the disk-full pages. |
| `tv-<env>-order-update-dropped` | data-at-risk-alarms.tf | Order updates reaching no consumer | KEEP | Data-loss page. |
| `tv-<env>-log-lines-dropped` | data-at-risk-alarms.tf | Log lines dropped | KEEP | Data-loss page: lost ERROR lines would hide other pages. |
| `tv-<env>-feed-thread-wrote-to-disk` | data-at-risk-alarms.tf | Feed thread writing a rescue file itself | REMOVE | Nothing is lost (rows written); a stall it causes is paged by `errcode-hot-path-stall-01`. |
| `tv-<env>-binary-sha-stale` | deploy-watchdog-lambda.tf | Running binary a day behind main | KEEP | Not in the KEPT list, kept because the watchdog Lambda publishes its metric directly; removing it needs a Lambda code change (its metric would be shipped and unread). Candidate for the next round. |
| `tv-<env>-deploy-watchdog-errors` | deploy-watchdog-lambda.tf | The deploy watchdog failing | REMOVE | Not on the trading or money path; a dead watchdog leaves `binary-sha-stale` quiet, accepted. |
| `tv-<env>-deploy-watchdog-not-invoked` | deploy-watchdog-lambda.tf | The deploy watchdog not running | REMOVE | Same as the row above. |
| `tv-<env>-deploy-provenance-blind` | deploy-watchdog-lambda.tf | Watchdog could not compare binaries | KEEP | Kept with `binary-sha-stale`: while the watchdog cannot compare binaries, `binary-sha-stale` reads OK on no data (noise lock §2.3q), so without this alarm a kept page would read OK when it does not know. (Removed in the first draft; put back after its review.) |
| `tv-<env>-dhan-depth-token-minter-errors` | dhan-depth-token-minter-lambda.tf | Depth-account token mint failing | KEEP | Token page; deployed only when the depth account is on (count 0 today, costs nothing). |
| `tv-<env>-dhan-depth-token-minter-not-invoked` | dhan-depth-token-minter-lambda.tf | Depth-account token mint not running | KEEP | Same as the row above. |
| `tv-<env>-dhan-token-minter-errors` | dhan-token-minter-lambda.tf | Daily Dhan token mint failing | KEEP | Token page (KEPT list). |
| `tv-<env>-dhan-token-minter-not-invoked` | dhan-token-minter-lambda.tf | Daily Dhan token mint not running | KEEP | Token page (KEPT list). |
| `tv-<env>-eventbridge-dlq-depth` | eventbridge-dlq.tf | Daily start or stop schedule dead-lettered | REMOVE | A missed start is paged by `boot-heartbeat-missing` and `market-hours-liveness-missing`; a missed stop by the kept budget pages (hard-stop guard, start watchdog). |
| `tv-<env>-errcode-stream-silent` | error-code-alarms.tf | Coded ERROR stream gone flat | KEEP | Every KEPT coded page (WS-SPILL-01/02, AGGREGATOR-DROP-01, PROC-01, DH-901, AUTH-GAP-04/05, BOOT-02, STORAGE-GAP-05, RISK-GAP-03, HOT-PATH-STALL-01, the universe-collapse and cross-verification pages) reads `$.code` from errors.jsonl. If only that file stops shipping, app.log keeps the log group alive, so `app-log-ingestion-silent` stays green; this is the only page for it (noise lock §2.3s forbids removing it on that ground). Its two schema filters stay. (Removed in the first draft; put back after its review.) |
| `tv-<env>-preopen-ready-late` | error-code-alarms.tf | Pre-open subscription finished late | REMOVE | Late, not lost; a lane that never comes up is paged by the lane-down pages. |
| `tv-<env>-errcode-hot-path-02` | error-code-alarms.tf | HOT-PATH-02 (rows dropped or table without DEDUP) | KEEP | Data-loss page. |
| `tv-<env>-errcode-dh-901` | error-code-alarms.tf | DH-901 token invalid | KEEP | Token page (KEPT list). |
| `tv-<env>-errcode-dh-906` | error-code-alarms.tf | DH-906 order error | REMOVE | No real orders are placed (paper mode). |
| `tv-<env>-errcode-auth-gap-04` | error-code-alarms.tf | AUTH-GAP-04 TOTP secret rotated | KEEP | Token page (KEPT list). |
| `tv-<env>-errcode-proc-01` | error-code-alarms.tf | PROC-01 out-of-memory kill | KEEP | A kill mid-session loses records still queued for the WAL (data loss) and is the only crash page left. |
| `tv-<env>-errcode-resource-02` | error-code-alarms.tf | RESOURCE-02 memory at 80% of its ceiling | REMOVE | Early warning; the kill itself is paged by `errcode-proc-01`. |
| `tv-<env>-errcode-aggregator-drop-01` | error-code-alarms.tf | AGGREGATOR-DROP-01 candle permanently lost | KEEP | Data-loss page. |
| `tv-<env>-errcode-aggregator-stall-01` | error-code-alarms.tf | AGGREGATOR-STALL-01 feed waited on a slow disk | KEEP | A Critical code: while the feed waits the vendor skips ticks with no count anywhere (live-lane data loss). Critical codes keep their page. |
| `tv-<env>-errcode-hot-path-stall-01` | error-code-alarms.tf | HOT-PATH-STALL-01 live path stalled 2 s | KEEP | Live-lane page: during a stall the vendor skips ticks with no count anywhere. |
| `tv-<env>-errcode-wal-suspend-01` | error-code-alarms.tf | WAL-SUSPEND-01 apply lag or suspension | REMOVE | A suspended table is paged by `questdb-wal-suspended` (the gauge alarm, kept); a blind probe (its `probe_blind` source) is paged by `wal-suspension-probe-blind` (kept). |
| `tv-<env>-errcode-auth-gap-05-remint-failed` | error-code-alarms.tf | Forced token re-mint failed | KEEP | Token page (KEPT list). |
| `tv-<env>-errcode-boot-02` | error-code-alarms.tf | BOOT-02 QuestDB boot probe timed out | KEEP | QuestDB-down page (KEPT list). |
| `tv-<env>-errcode-boot-03` | error-code-alarms.tf | BOOT-03 clock skew blocked boot | KEEP | A Critical code: the app refuses to start, so no data is captured; the page names the cause. |
| `tv-<env>-errcode-oms-gap-06` | error-code-alarms.tf | OMS-GAP-06 paper order runtime respawned | KEEP | A Critical code: paper positions and day P&L were silently zeroed; Critical codes keep their page. |
| `tv-<env>-errcode-ws-spill-01` | error-code-alarms.tf | WS-SPILL-01 WAL writer disk error | KEEP | Data-loss page. |
| `tv-<env>-errcode-ws-spill-02` | error-code-alarms.tf | WS-SPILL-02 frame dropped at the WAL | KEEP | Data-loss page. |
| `tv-<env>-errcode-storage-gap-05` | error-code-alarms.tf | STORAGE-GAP-05 nothing left to reclaim | KEEP | Disk-full page (KEPT list). |
| `tv-<env>-errcode-risk-gap-03` | error-code-alarms.tf | RISK-GAP-03 subscribed instruments silent | KEEP | Live-lane page: ticks missing for instruments we hold (the 1% floor, noise lock §2.19). |
| `tv-<env>-errcode-ws-gap-03-universe-collapse` | error-code-alarms.tf | Live feed fell back to 4 indices | KEEP | Live-lane page: almost the whole subscription lost. |
| `tv-<env>-errcode-ws-gap-03-xverify-vacuous` | error-code-alarms.tf | Cross-verification compared nothing | KEEP | Cross-verification page (KEPT list). |
| `tv-<env>-errcode-ws-gap-03-xverify-failed` | error-code-alarms.tf | Cross-verification did not run | KEEP | Cross-verification page (KEPT list). |
| `tv-<env>-errcode-ws-gap-03-xverify-diverged` | error-code-alarms.tf | Cross-verification found mass divergence | KEEP | Cross-verification page (KEPT list). |
| `tv-<env>-errcode-scoreboard-01` | error-code-alarms.tf | SCOREBOARD-01 daily rollup not written | REMOVE | A summary table; the raw audit rows it rolls up are kept and covered by `audit-rows-lost`. |
| `tv-<env>-dhan-live-lane-down` | live-lane-alarms.tf | Live lane not carrying data | KEEP | Lane-down page (KEPT list). |
| `tv-<env>-dhan-socket-parked` | live-lane-alarms.tf | A socket parked for the session | KEEP | Socket-parked page (KEPT list). |
| `tv-<env>-ticks-dropped` | live-lane-alarms.tf | Ticks lost before QuestDB | KEEP | Ticks-dropped page (KEPT list). |
| `tv-<env>-ws-ring-full` | live-lane-alarms.tf | Frame ring refused frames (slots) | KEEP | Data-loss page. |
| `tv-<env>-ws-ring-bytes-full` | live-lane-alarms.tf | Frame ring refused frames (bytes) | KEEP | Data-loss page. |
| `tv-<env>-live-universe-fallback` | live-lane-alarms.tf | Lane not using today's list | KEEP | Not a duplicate of `errcode-ws-gap-03-universe-collapse`: that filter matches only `fell_back_to_indices`, while this counter also counts `truncated_to_capacity` (the owner's D3a "fill 25,000 by priority with a critical page"), `artifact_unreadable`, `artifact_unparseable` and `no_usable_widening`. In each of those, contracts go unsubscribed for the session (data never captured). (Removed in the first draft; put back after its review.) |
| `tv-<env>-ws-no-alive-connections` | live-lane-alarms.tf | Every socket gone | KEEP | Lane-down page. |
| `tv-<env>-wal-frames-not-recovered` | live-lane-alarms.tf | Replayed frames not recovered | KEEP | Data-loss page. |
| `tv-<env>-dhan-wal-dropped` | live-lane-alarms.tf | Frame missed the WAL | KEEP | Data-loss page. |
| `tv-<env>-ticks-lost-spill` | live-lane-alarms.tf | Ticks lost after the rescue spill | KEEP | Data-loss page. |
| `tv-<env>-dhan-no-ticks-flowing` | live-lane-alarms.tf | No tick persisted for 10 min in session | KEEP | Lane-down page. |
| `tv-<env>-dhan-contract-universe-failed` | live-lane-alarms.tf | Option contracts not subscribed | KEEP | Live-lane page: no option data at all for the day. |
| `tv-<env>-tick-spill-replay-failing` | live-lane-alarms.tf | Rescued ticks not draining back | REMOVE | Rows are kept on disk, not lost; disk pressure is paged by the disk-full pages. |
| `tv-<env>-ticks-spilling` | live-lane-alarms.tf | A tick flush rescued to disk | REMOVE | Nothing lost (the rescue worked); a real loss is paged by `ticks-dropped`. |
| `tv-<env>-dhan-worst-socket-deaf` | live-lane-alarms.tf | One socket silent for 10 min | REMOVE | A deaf socket silences its instruments, which `errcode-risk-gap-03` pages once 1% of tracked instruments go quiet. |
| `tv-<env>-wal-replay-unknown-magic` | live-lane-alarms.tf | WAL record format unreadable | KEEP | Data-loss page. |
| `tv-<env>-seal-writer-rescued` | live-lane-alarms.tf | Candles escalated to disk | REMOVE | Nothing lost; a real candle loss is paged by `seal-writer-dropped` and `errcode-aggregator-drop-01`. |
| `tv-<env>-aggregator-slots-exhausted` | live-lane-alarms.tf | An instrument refused a candle slot | KEEP | Data-loss page: that instrument gets no candles. |
| `tv-<env>-tick-spill-quarantined` | live-lane-alarms.tf | A rescued tick file set aside | KEEP | Data-loss page: a whole quarantined file (often thousands of good rows) is not in QuestDB and is never replayed automatically. `market-data-refused` counts only single refused lines, not whole files, and TICK-SPILL-01 has no filter. (Removed in the first draft; put back after its review.) |
| `tv-<env>-wal-catchup-budget-exhausted` | live-lane-alarms.tf | Boot catch-up ran out of time | REMOVE | Nothing lost; segments stay on disk for the next boot. |
| `tv-<env>-offload-writer-shutdown-incomplete` | live-lane-alarms.tf | Writer died with batches at shutdown | KEEP | Data-loss page. |
| `tv-<env>-tick-rescue-abandoned` | live-lane-alarms.tf | Rescue thread missed the shutdown budget | KEEP | Data-loss page. |
| `tv-<env>-wal-spill-shutdown-incomplete` | live-lane-alarms.tf | WAL writer abandoned with frames queued | KEEP | Data-loss page. |
| `tv-<env>-wal-replay-restore-failed` | live-lane-alarms.tf | Deferred WAL segment not moved back | REMOVE | A DATA-LOSS path, not a harmless one: a segment that cannot be moved back stays in `replaying/`, where the confirm step archives it unread, so its frames never reach the database. The page is kept: the same site logs a coded `WS-SPILL-02` ERROR, which `tv-<env>-errcode-ws-spill-02` (KEPT) matches with no source filter. That filter must stay unnarrowed; test 10 of `quote53_alarm_review_cover_guard.rs` pins the site, the filter and this row. |
| `tv-<env>-aggregator-refusal-rate-high` | live-lane-alarms.tf | A quarter of ticks refused a candle bucket | REMOVE | Rows are still written; not loss. |
| `tv-<env>-dhan-ring-dwell-high` | live-lane-alarms.tf | Frames waiting long in the ring | REMOVE | Early warning; refused frames are paged by the ring-full pages. |
| `tv-<env>-dhan-feed-delay-high` | live-lane-alarms.tf | Index prices arriving a minute late | REMOVE | Delay, not loss (noise lock §2.6 family; note added). |
| `tv-<env>-dhan-main-reconnect-slow` | live-lane-alarms.tf | Main-feed reconnect took 15 s or more | REMOVE | Delay, not loss (noise lock §2.6 family; note added). A socket that does not come back is paged by `dhan-socket-parked` and `ws-no-alive-connections`. |
| `tv-<env>-logs-ingestion-runaway` | log-retention.tf | Over 5 GB of logs in a day | REMOVE | Cost signal; the $75 budget stop and the kept budget pages bound it. |
| `tv-<env>-app-log-ingestion-silent` | log-retention.tf | App log shipping silent in session | KEEP | Every coded page above travels this log route; without it those pages go dark. |
| `tv-<env>-partition-archive-failed` | loss-and-retention-alarms.tf | QuestDB partition copy to S3 failing | REMOVE | Nothing is dropped without a verified copy, so data is kept; the disk-full pages catch the growth. |
| `tv-<env>-market-data-persistence-loss` | loss-and-retention-alarms.tf | Tick or depth rows did not reach QuestDB | KEEP | Data-loss page. |
| `tv-<env>-durable-floor-breach` | loss-and-retention-alarms.tf | The WAL itself failed | KEEP | Data-loss page. |
| `tv-<env>-wal-suspension-probe-blind` | loss-and-retention-alarms.tf | WAL probe blind for several polls | KEEP | The only blind detector left for the kept `questdb-wal-suspended` page: when `wal_tables()` fails while QuestDB still answers, the gauge reads -1, `questdb-wal-suspended` (needs at least 1) stays green and `questdb-disconnected` stays green too. Reads a gauge already shipped. (Removed in the first draft; put back after its review.) |
| `tv-<env>-market-data-refused` | loss-group-alarms.tf | Loss group: market data refused | KEEP | Loss-group page (KEPT list). |
| `tv-<env>-subscription-coverage-lost` | loss-group-alarms.tf | Loss group: contracts with no data | KEEP | Loss-group page (KEPT list). |
| `tv-<env>-audit-rows-lost` | loss-group-alarms.tf | Loss group: audit rows lost | KEEP | Loss-group page (KEPT list). |
| `tv-<env>-durability-sync-failing` | loss-group-alarms.tf | Loss group: disk syncs failing | KEEP | Loss-group page (KEPT list). |
| `tv-<env>-order-path-dropped` | loss-group-alarms.tf | Loss group: order path drops | KEEP | Loss-group page (KEPT list). |
| `tv-<env>-safety-bound-or-monitor-blind` | loss-group-alarms.tf | Loss group: full tables or blind checks | KEEP | Loss-group page (KEPT list). |
| `tv-<env>-market-hours-liveness-missing` | market-hours-liveness-alarm.tf | No tick-age reading in session (dead box) | KEEP | Lane-down page: the only page for a box that is down or dead in session. |
| `tv-<env>-market-hours-gate-errors` | market-hours-liveness-alarm.tf | The market-hours gate failing | KEEP | Arms the lane-down pages; without it they stay disarmed. |
| `tv-<env>-market-hours-gate-not-invoked` | market-hours-liveness-alarm.tf | The market-hours gate not running | KEEP | Same as the row above. |
| `tv-<env>-market-open-readiness-errors` | market-open-readiness-lambda.tf | The 08:45 readiness pager failing | REMOVE | Early warning; an app that is not up is still paged by `boot-heartbeat-missing` and the liveness page. |
| `tv-<env>-market-open-readiness-not-invoked` | market-open-readiness-lambda.tf | The 08:45 readiness pager not running | REMOVE | Same as the row above. |
| `tv-<env>-operator-control-errors` | operator-control-lambda.tf | Operator control Lambda failing | REMOVE | It paged nobody (no alarm actions); a failing button shows itself to the operator. |
| `tv-<env>-orders-placed-storm` | order-side-alarms.tf | Order placement storm | KEEP | Money path: kept so a runaway placement pages the day orders go live; its guard pins it. |
| `tv-<env>-daily-loss-breach` | order-side-alarms.tf | Daily P&L past the loss limit | KEEP | Money path: the daily-loss page; its guard pins it. |
| `tv-<env>-order-audit-chain-loss` | order-side-alarms.tf | Order audit rows lost | KEEP | Data-loss page (SEBI audit rows). |
| `tv-<env>-questdb-console-front-errors` | questdb-console.tf | QuestDB console front Lambda failing | REMOVE | It paged nobody (no alarm actions); a failing console shows itself to its user. |
| `tv-<env>-questdb-console-proxy-errors` | questdb-console.tf | QuestDB console proxy Lambda failing | REMOVE | Same as the row above. |
| `tv-<env>-seal-writer-dropped` | seal-drop-alarm.tf | Finished candles thrown away | KEEP | Data-loss page. |
| `tv-<env>-start-watchdog-errors` | start-watchdog-lambda.tf | The start watchdog failing | KEEP | Budget page: it also runs the daily stop and the stop check. |
| `tv-<env>-start-watchdog-not-invoked` | start-watchdog-lambda.tf | The start watchdog not running | KEEP | Budget page, and a guard pins it by name as the box-starter. |
| `tv-<env>-telegram-drops` | telegram-drop-alarm.tf | Typed Telegram pages dropped | KEEP | Page delivery: a broken bot silences the token Critical pages. |
| `tv-<env>-telegram-webhook-errors` | telegram-webhook-lambda.tf | The Lambda that sends every alarm to Telegram failing | KEEP | Page delivery for every alarm here. |

## Metrics shipped to CloudWatch (Quote 53 "Trim CloudWatch metrics")

Rule: CloudWatch receives only the metrics an alarm reads. Every metric stays on
the box's local `/metrics`, and the metrics log group still receives every
scrape, so the loss-group metric filters (which read that log group) are not
touched.

**Delivery, and why this change merges only on or after 1 December:** the agent
config reaches the box with no terraform in the path (`deploy-aws.yml` copies it
on every merge that touches it, and every morning boot copies the repo copy), so
the terraform date gate cannot hold it back. Merged in November, it would stop
shipping these metrics while the gated terraform kept every alarm that reads
them, and those alarms (`notBreaching`) would sit green on missing data.
Test 9 of `quote53_alarm_review_cover_guard.rs` fails the build while the
trimmed config is in the tree before 2026-12-01 00:00 IST.

| Item | Decision | Plain reason |
|---|---|---|
| 63 EMF names in `deploy/aws/cloudwatch-agent.json` (listed below) | REMOVE | No kept alarm reads them (most were read only by the deleted dashboard or a removed alarm). |
| The `class`-dimensioned series `tv_dhan_ws_lag_max_ms_by_class` (second EMF declaration) | REMOVE | Its only reader, `dhan-feed-delay-high`, is removed. |
| Host metrics `mem_used_percent` and `cpu_usage_active` (agent `metrics_collected`, and the boot fallback in `user-data.sh.tftpl`) | REMOVE | Their only reader, `mem-used-high`, is removed; `cpu-high-5min` read the free EC2 metric. `disk used_percent` stays (read by `disk-used-high`). |
| Metric filter `deploy_provenance_blind` | KEEP | Read by the kept `deploy-provenance-blind`. |
| Metric filter `preopen_ready` (`tv_dhan_preopen_ready_secs`) | REMOVE | Its alarm is removed. |
| Metric filters `log_schema_flat_errors` and `log_schema_nested_errors` | KEEP | Read by the kept `errcode-stream-silent`. |
| Metric filter `log_lines_dropped_shipped` (`tv_log_lines_dropped_shipped_total`) | KEEP, flagged | No alarm reads it yet: it is the measurement noise lock §2.9-i lands before switching `log-lines-dropped` to it. One metric, about $0.30 a month. The owner decides: switch the alarm, or remove the filter. |
| Metric `tv_binary_main_sha_mismatch` (published by the deploy watchdog Lambda itself) | KEEP | Read by the kept `binary-sha-stale`. |

The 46 names kept are pinned by
`crates/common/tests/cloudwatch_app_alarms_wiring.rs::test_emf_metric_selectors_name_count_is_pinned`,
and `crates/common/tests/dashboard_live_lane_visibility_guard.rs` fails the build
if any shipped name has no alarm reading it.

EMF names removed (63; `tv_dhan_live_universe_fallback_total` and `tv_tick_spill_replay_quarantined_total` stay, read by the kept fallback and quarantine pages):

`tv_orders_rejected_total`, `tv_clock_skew_seconds`, `tv_disk_watcher_respawn_total`, `tv_process_rss_bytes`, `tv_questdb_wal_apply_lag_max`, `tv_questdb_reconnects_total`, `tv_partition_archive_failed_total`, `tv_dhan_order_push_respawn_total`, `tv_open_fds`, `tv_oom_kills_total`, `tv_ram_store_dropped_total`, `tv_ram_store_errors_total`, `tv_mid_session_profile_rest_degraded_total`, `tv_boot_deadline_exceeded_total`, `tv_dhan_ws_frame_refused_total`, `tv_dhan_ws_subscribe_failed_total`, `tv_dhan_feed_seals_dropped_total`, `tv_dhan_feed_seals_emitted_total`, `tv_dhan_feed_seq_refused`, `tv_dhan_feed_instruments_silent`, `tv_dhan_feed_instruments_never_ticked`, `tv_dhan_feed_drain_frames_total`, `tv_dhan_feed_ingest_ticks_total`, `tv_dhan_feed_ingest_refused_total`, `tv_dhan_ws_reconnect_total`, `tv_dhan_ws_dial_failed_total`, `tv_dhan_feed_stack_connections`, `tv_dhan_ws_lag_excluded_total`, `tv_dhan_feed_ring_max_bytes`, `tv_host_total_ram_bytes`, `tv_dhan_feed_depth_total`, `tv_tick_spill_replay_failed_total`, `tv_wal_suspension_probe_failed_total`, `tv_candle_session_high_recovered_total`, `tv_candle_session_low_recovered_total`, `tv_dhan_feed_ring_dwell_max_ms`, `tv_dhan_ws_lag_max_ms`, `tv_dhan_ws_worst_conn_tick_age_secs`, `tv_dhan_feed_ring_resident_pct`, `tv_wal_replay_recovered_total`, `tv_wal_replay_deferred_segments_total`, `tv_dhan_wal_refolded_total`, `tv_aggregator_tick_refused_total`, `tv_candle_tick_discarded_late_total`, `tv_dhan_feed_abandoned_bytes_total`, `tv_depth_spill_write_errors_total`, `tv_dhan_feed_seals_rescued_total`, `tv_wal_catchup_budget_exhausted_total`, `tv_seal_escalation_queued_total`, `tv_seal_escalation_inline_fallback_total`, `tv_seal_escalation_lost_total`, `tv_seal_escalation_abandoned_total`, `tv_seal_spill_write_errors_total`, `tv_tick_spill_over_soft_ceiling_total`, `tv_depth_spill_over_soft_cap_total`, `tv_tick_optional_price_dropped_total`, `tv_wal_replay_restore_failed_total`, `tv_dhan_ws_main_reconnect_recovery_max_ms`, `tv_wal_prune_refused_not_uploaded_total`, `tv_seal_spill_prune_refused_not_uploaded_total`, `tv_quarantine_prune_refused_not_uploaded_total`, `tv_tick_rescue_inline_fallback_total`, `tv_depth_rescue_inline_fallback_total`

## The CloudWatch dashboard (Quote 53 "Delete the CloudWatch dashboard")

`deploy/aws/terraform/dashboard.tf` and its `dashboard_url` output are deleted.
This reverses Quote 37's "dashboard KEPT" (dated notes added where the rules said
it is kept). The operator portal and Telegram stay.

## Raw frames in S3 (Quote 53 "Cheaper S3 class for old raw frames")

The `tv_cold` lifecycle rule for `raw-frames/` now moves current and noncurrent
versions to S3 Glacier Instant Retrieval after 30 days (rule
`raw-frames-glacier-ir`, in `deploy/aws/terraform/main.tf`). Nothing is deleted
or expired; versioning is unchanged; no new rule touches `questdb-partitions/`,
`deploys/`, `instrument-snapshots/` or `logs/`.

**Risk, for the owner:** before this change the same rule sent `raw-frames/` to
Deep Archive after 30 days, which costs less per GB than Glacier Instant
Retrieval (AWS list prices, Mumbai, Assumed: about $0.002 against $0.005 per
GB-month), but needs a restore of about 12 hours before a read. So this
transition makes raw frames readable at once and costs more than the rule it
replaces, not less: at about 165 GB of new raw frames a month, about $0.50 more
per month for each month of data kept (Assumed). An object already in Deep
Archive stays there (lifecycle cannot move it back up). If the owner meant
"cheaper than today", the old Deep Archive rule was cheaper.

**Second risk, unchanged by this PR:** the bucket-wide `tick-cold-tiering` rule
(no prefix) has moved every object, `questdb-partitions/` included, to
Intelligent-Tiering after 30 days and Glacier Instant Retrieval after 365 days
since 2026-09-29. Quote 53 says `questdb-partitions/` "stays STANDARD"; it is not
STANDARD today. Both classes are readable at once. This PR does not change that
rule, because Quote 53 authorizes no change to it; the owner decides.

## Cost estimate (Assumed, never measured)

At AWS list prices (Assumed): a standard alarm about $0.10 a month (a metric-math
alarm is billed per metric it reads), a custom metric about $0.30 a month, a
dashboard $3 a month. Removing 37 alarms saves about $4.50 to $5 a month (more for
the removed metric-math alarms); removing 63 EMF names, the class series, two
host metrics and one filter metric saves up to about $21 a month if each was
publishing every month (many publish only when they change, so the real figure
is lower; Quote 53's own estimate for all shipped metrics was about $4.90); the
dashboard saves $3. Against Quote 53's estimate (alarms about $9.40, shipped
metrics about $4.90, dashboard $3), the total saving is about $11 to $14 a month
before tax, and the raw-frame class change adds about $0.50 a month per month
of data kept. None of this is measured; read the December bill.

## Alarms kept only by a broad reading (owner may approve removing them)

| Alarm | Why kept |
|---|---|
| `tv-<env>-api-auth-failed` | The only page for a password-guessing sweep on the operator API (security). |
| `tv-<env>-binary-sha-stale` | Removing it needs the deploy watchdog Lambda to stop publishing its metric (code change). |
| `tv-<env>-instance-status-failed`, `tv-<env>-system-status-failed` | Each carries an EC2 self-heal action (reboot, recover). |
| `tv-<env>-errcode-aggregator-stall-01`, `tv-<env>-errcode-boot-03`, `tv-<env>-errcode-oms-gap-06` | Severity Critical codes; a guard requires every Critical code with an emit site to page. |
| `tv-<env>-errcode-proc-01` | Out-of-memory kill; read as data loss (queued WAL records die with the process). |
| `tv-<env>-errcode-hot-path-stall-01` | Stall; read as data loss (the vendor skips ticks during it). |
| `tv-<env>-errcode-risk-gap-03` | Silent instruments; read as a live-lane data gap. |
| `tv-<env>-dhan-contract-universe-failed` | No options subscribed; read as a live-lane data gap. |
| `tv-<env>-aggregator-slots-exhausted` | One instrument loses its candles; read as data loss. |
| `tv-<env>-log-lines-dropped`, `tv-<env>-order-update-dropped` | Data-at-risk family; read as data loss. |
| `tv-<env>-daily-budget-digest-errors`, `tv-<env>-daily-budget-digest-not-invoked` | Read as budget pages. |
| `tv-<env>-telegram-drops`, `tv-<env>-telegram-webhook-errors`, `tv-<env>-app-log-ingestion-silent` | They carry every other page; without them a kept page can go silent. |
| `tv-<env>-market-hours-gate-errors`, `tv-<env>-market-hours-gate-not-invoked` | They arm the lane-down pages. |
| `tv-<env>-start-watchdog-errors`, `tv-<env>-start-watchdog-not-invoked` | Budget (daily stop) and box start; a guard pins the did-it-run alarm by name. |

## Before any real-money order

`tv-<env>-orders-rejected` and `tv-<env>-errcode-dh-906` were removed because no
real order is placed (paper mode, `dry_run`). They must come back, with a fresh
dated quote, in the same change that allows a real-money order. The daily-loss
and order-storm pages are kept.
