# AWS Daily Lifecycle — 8:30 AM Auto-Start + 12 Extreme Worst Cases

> **Status:** DESIGN DOC (no code shipped). Discussion artefact.
> **Authority:** CLAUDE.md > operator-charter-forever.md > aws-budget.md > this file.
> **Scope:** The lifecycle from EventBridge cron firing at 08:30 IST through the app's first heartbeat. Once the app is healthy, hand off to `operator-daily-startup.md` and `aws-disaster-recovery.md`.
> **Created:** 2026-05-18.
> **Mode:** Honest-envelope design per operator-charter §F. No "never fails" promises.

---

## §0. Auto-driver one-liner

> "Sir, imagine your shop has 6 alarm clocks. Old way: 1 clock. If it breaks, you sleep through. New way: 1 phone alarm + 1 SMS from the watchman + 1 email from the bank + 1 Telegram from the shop CCTV + 1 phone call from the security agency + 1 watchdog dog. If ANY ONE works, you wake up by 8:35 AM. Five must break simultaneously for you to miss. That's the design."

ASCII diagram of the 6 independent channels (each survives without the others):

```
┌─────────────────────────────────────────────────────────────────────────┐
│ 08:30 IST cron tick from EventBridge (AWS-side, NOT our app)            │
└────────────────────────────────┬────────────────────────────────────────┘
                                 ▼
              ┌──────────────────────────────────┐
              │  SSM Automation StartEC2Instance │ ← deadman channels listen
              └──────────────────────────────────┘    here, NOT inside app
                                 │
        ┌────────────────────────┼────────────────────────┐
        ▼                        ▼                        ▼
   [STARTS OK]              [STARTS BUT          [NEVER STARTS]
                            CRASHES MID-BOOT]
        │                        │                        │
        ▼                        ▼                        ▼
   App emits           CloudWatch SystemCheck      Scheduler.TargetErrorCount
   ✅ Telegram         alarm @ T+5min →            > 0 alarm @ T+2min →
   from teloxide       SNS → 3 fanouts             SNS → 3 fanouts
   (in-band)           (out-of-band)               (out-of-band)
        │
        ▼                        SNS fanout (always 3 legs):
   Operator sees           ┌──────┬──────┬──────────────┐
   ✅ by 08:33 IST         ▼      ▼      ▼              ▼
                          SMS   Email   Telegram      [optional]
                                       webhook        2nd-region
                                                      SNS topic

Operator sees a signal by 08:35 IST in the worst case OR by 08:33 IST in the happy case.
```

---

## §1. The happy path (T+0 → T+180s)

| T+sec | Where | Event | Visible to operator |
|---|---|---|---|
| 0 | EventBridge | `weekday_start` rule fires `cron(30 2 ? * MON-FRI *)` — 02:30 UTC = 08:00 IST per `main.tf:256` **OR** 03:00 UTC = 08:30 IST per `aws-budget.md` (DRIFT — see §A) | — (AWS internal) |
| 0–5 | SSM Automation | `AWS-StartEC2Instance` invokes `ec2:StartInstances` | CloudWatch event `EC2 Instance State-change Notification` → `pending` |
| 5–45 | EC2 | Hypervisor allocates, instance boots Linux, cloud-init runs systemd `tickvault.service` | EventBridge state-change → `running` |
| 45–90 | systemd | `docker compose up -d` (8 containers) | App-side tracing |
| 90–150 | tickvault | 15-step boot per `crates/app/src/main.rs:11-25` — auth, QuestDB DDL, universe, WS pool | App-side Telegram once boot complete |
| 150–180 | tickvault | `BootReadyConfirmation` Telegram emitted | ✅ **Operator sees green at ~08:33 IST** |
| 180+ | tickvault | At 09:14:30 IST: `Phase2ReadinessPassed`. At 09:15:30 IST: `MarketOpenStreamingConfirmation`. At 09:16:30 IST: `SelfTestPassed`. | 3 positive pings before market opens |

**Happy-path SLO:** operator's phone has a green Telegram from the app by **T+180s** = **08:33 IST** (or 09:03 IST under 08:30 IST schedule). Anything later is degraded.

---

## §2. Notification architecture — IN-BAND vs OUT-OF-BAND

The single most important design decision in this document:

| Track | Who sends | When it works | When it fails | Purpose |
|---|---|---|---|---|
| **IN-BAND** | tickvault app (teloxide → Telegram) | App is alive + has token + network | App is dead, OOM, boot-stuck, segfault | Rich detail messages (15-row matrices, exact SIDs, audit links) |
| **OUT-OF-BAND** | AWS-side (CloudWatch Alarm → SNS) | Anytime EC2/AWS plane is up | Region-wide AWS outage | Dead-man switch. Survives app death. |

**Rule:** Boot/AWS-level alerts MUST go OUT-OF-BAND. In-band Telegram is a finer-grain secondary. NEVER route a boot-failure alert through the app — the app is the thing that's broken.

### SNS fan-out (single topic → 3 independent legs)

```
            SNS topic: tv-prod-alerts (main.tf:229)
                        │
       ┌────────────────┼────────────────┐
       ▼                ▼                ▼
  SMS subscription   Email subs    HTTPS subscription
  (local route       (operator     (Telegram bot
  India: $0.00278/   email)        webhook)
   msg via DLT)
       │                │                │
       ▼                ▼                ▼
   Phone SMS       Inbox alert    Telegram chat (same chat as in-band)
```

Each leg is independent. SMS failing does not block Email; Email failing does not block Telegram-webhook.

### Notification SLA matrix (research-anchored, see §C)

| Leg | Documented SLA | Observed p50 | Failure mode |
|---|---|---|---|
| CloudWatch alarm state-change → SNS | <1s after alarm evaluation | typically <500ms | regional SNS outage (rare; no ap-south-1 SNS outage in last 12mo per AWS Health) |
| SNS → SMS (India local DLT) | none documented | typically 5–30s | carrier-side delivery delays, DND violations, DLT template mismatch |
| SNS → Email | none documented | typically 5–60s | spam filter, mail server down |
| SNS → HTTPS (Telegram webhook) | 50 attempts over ≤3600s | typically <2s | Telegram 429 rate limit, bot blocked, webhook URL changed |

**Honest envelope:** delivery of AT LEAST ONE leg within 60s is the operational target. AWS does not publish a numeric SLA for any of the three. Empirical p99 must be measured per-account.

---

## §3. The 12 EXTREME worst cases

Each case follows the same 7-field template:

```
(a) Symptom        — what the operator observes
(b) Root cause     — what actually went wrong
(c) Detection      — which AWS primitive notices, in how many seconds
(d) Routing        — how the signal reaches the operator
(e) Auto-recovery  — what AWS/our infra attempts before paging
(f) Operator action — what the human does
(g) Runbook target — existing or new docs/runbooks/*.md
```

### Case A — EventBridge rule itself disabled / deleted

| Field | Value |
|---|---|
| Symptom | 08:30 IST passes, EC2 stays stopped, no Telegram from app |
| Root cause | Operator/Terraform/IAM accident disabled `weekday_start` rule, OR cross-account IAM revoked |
| Detection | CloudWatch metric `AWS/Events Invocations` for rule `tv-prod-weekday-start` = 0 over the 08:25-08:35 window → CW alarm "no invocation in expected slot" → SNS |
| Routing | Out-of-band (SNS only — app never started) |
| Auto-recovery | None possible. EventBridge cannot resurrect its own rule. |
| Operator action | SSH from phone hotspot or web console: `aws events enable-rule --name tv-prod-weekday-start`. Manual `aws ec2 start-instances` for today. |
| Runbook | NEW STUB: `docs/runbooks/aws-eventbridge-rule-missing.md` |

**Acceptance criterion:** operator notified by 08:37 IST (T+7) even if EventBridge silently disabled overnight.

### Case B — StartInstances API throttle / IAM permission revoked

| Field | Value |
|---|---|
| Symptom | EventBridge fired, but instance still stopped |
| Root cause | IAM role `eventbridge_ec2_scheduler` (main.tf:298) lost `ec2:StartInstances` permission, OR an AWS-side throttle hit |
| Detection | EventBridge metric `TargetErrorCount > 0` (`AWS/Scheduler`) → CW alarm → SNS; ALSO the rule's DLQ (SQS standard) receives the failed invocation |
| Routing | Out-of-band SNS + DLQ depth alarm |
| Auto-recovery | EventBridge retries per `RetryPolicy.MaximumRetryAttempts` (currently UNSET in main.tf — must set to ≥3 with `MaximumEventAgeInSeconds = 900`) |
| Operator action | Check CloudTrail for `AssumeRole` / `StartInstances` denial; restore IAM policy; rerun. |
| Runbook | NEW STUB: `docs/runbooks/aws-startinstances-failed.md` |

**Gap:** `main.tf:253-275` does NOT set `RetryPolicy` on the rules — must add.

### Case C — EC2 capacity error (no t4g.medium in ap-south-1)

| Field | Value |
|---|---|
| Symptom | EventBridge fired, IAM is fine, but `StartInstances` returns `InsufficientInstanceCapacity` |
| Root cause | AWS-side capacity exhaustion in the AZ |
| Detection | SSM Automation execution status = `Failed` → CloudWatch event → SNS |
| Routing | Out-of-band |
| Auto-recovery | SSM Automation `AWS-StartEC2Instance` has no built-in fallback AZ. Must extend with a fallback document that retries in alternate AZ. |
| Operator action | Manual: `aws ec2 stop-instances` (cleanup) → `modify-instance-attribute --instance-type c7i.large` (smaller fallback) → start. Or wait + retry. |
| Runbook | NEW STUB: `docs/runbooks/aws-capacity-error.md` |

**Honest envelope:** AWS does not guarantee single-AZ capacity. Mitigation = multi-AZ design OR accept a degraded-fallback day. **Operator decision needed.**

### Case D — Instance starts but cloud-init / user-data fails

| Field | Value |
|---|---|
| Symptom | Instance is `running` per AWS, but tickvault never starts. SSH works. |
| Root cause | `user-data.sh.tftpl` broke — git clone failed, docker install failed, systemd unit not enabled |
| Detection | `/var/log/cloud-init-output.log` has the error BUT is invisible to CloudWatch unless CW agent pre-tails it (research §D #13). Plus: app never registers `tv_boot_complete = 1` custom metric → CW alarm "boot not complete by T+180s" → SNS |
| Routing | Out-of-band — the "boot-not-complete" alarm is the catch-all |
| Auto-recovery | systemd `RestartSec=3` retries the unit. No effect if user-data ran once and failed irrecoverably (user-data is first-boot only on a fresh AMI). |
| Operator action | SSH; `cat /var/log/cloud-init-output.log`; fix; manually `systemctl start tickvault`. |
| Runbook | NEW STUB: `docs/runbooks/aws-cloudinit-failure.md` |

**Acceptance criterion:** alarm fires within 60s of T+180s deadline.

### Case E — Instance starts, docker daemon dead

| Field | Value |
|---|---|
| Symptom | EC2 running, SSH works, but `docker ps` errors |
| Root cause | Docker daemon crash or `/var/lib/docker` corruption from prior unclean shutdown |
| Detection | tickvault.service `Type=notify` watchdog never receives `READY=1` → systemd restarts; after 3 restarts → systemd halts → app-not-running alarm |
| Routing | Same "boot-not-complete by T+180s" alarm as Case D |
| Auto-recovery | systemd retries; user-data installer attempts `systemctl restart docker` (NOT currently in main.tf — must add) |
| Operator action | SSH; `journalctl -u docker --since "5 min ago"`; `systemctl restart docker`; `docker compose up -d`. |
| Runbook | NEW STUB: `docs/runbooks/aws-docker-daemon-dead.md` (NOTE: overlaps with `aws-disaster-recovery.md` §1) |

### Case F — Docker fine, tickvault binary fails (config error / corrupt binary)

| Field | Value |
|---|---|
| Symptom | All containers up, but `tickvault` exits with non-zero |
| Root cause | Bad config TOML, corrupt binary from deploy, missing SSM secret, glibc mismatch |
| Detection | systemd records exit code → `Restart=on-failure` retries 3× → halt → CW Logs subscription filter on `tickvault.service` failure → SNS |
| Routing | Out-of-band (CW Logs subscription filter MUST be added — currently aspirational) |
| Auto-recovery | systemd 3-retry burst, then stop |
| Operator action | SSH; `journalctl -u tickvault -n 200`; identify error; fix config or rollback binary. |
| Runbook | NEW STUB: `docs/runbooks/aws-tickvault-exit-loop.md` |

### Case G — Tickvault starts but Dhan auth fails

| Field | Value |
|---|---|
| Symptom | App boots far enough to emit logs, but boot halts at Step 7 (auth) |
| Root cause | TOTP secret rotated externally (AUTH-GAP-04), JWT generation rejected, IP changed (Static IP mandate effective 2026-04-01) |
| Detection | App emits `error!` with `code = "DH-901"` or `code = "AUTH-GAP-*"` → in-band Telegram (app IS alive enough) → AND CW Logs filter on `code = "AUTH-GAP-*"` → SNS as backup |
| Routing | In-band Telegram (primary, app alive) + Out-of-band SNS (backup) |
| Auto-recovery | Token manager retries cache → SSM → TOTP regen. After 3 failures → HALT. |
| Operator action | Verify SSM TOTP secret unchanged; regenerate via Dhan web UI if rotated; update SSM. |
| Runbook | EXISTING: `docs/runbooks/auth.md` |

### Case H — Tickvault starts but QuestDB unreachable

| Field | Value |
|---|---|
| Symptom | App boots, halts at Step 8 (QuestDB DDL) |
| Root cause | docker compose did not start `tv-questdb`, port collision, EBS volume corruption |
| Detection | `BOOT-01` ERROR at T+30s, `BOOT-02` HALT at T+60s — both routed via in-band Telegram. Backup: app never emits `tv_boot_complete = 1` → out-of-band alarm. |
| Routing | In-band + out-of-band parallel |
| Auto-recovery | `wait_for_questdb_ready` polls 60s. systemd retry of `tickvault.service` may re-trigger compose. |
| Operator action | `docker logs tv-questdb`; check EBS volume health; `docker compose restart tv-questdb`. |
| Runbook | EXISTING: `docs/runbooks/aws-disaster-recovery.md` §1 |

### Case I — Tickvault starts but WS connect fails

| Field | Value |
|---|---|
| Symptom | App is up and persisting, but no live ticks |
| Root cause | Dhan-side outage, static IP mismatch, JWT rejected by WS handshake |
| Detection | App emits `code = "WS-GAP-*"` → in-band Telegram. Also: `tv_websocket_connections_active = 0` for >60s → Prom alert → Alertmanager → Telegram |
| Routing | In-band Telegram (app alive — primary). Out-of-band backup if Prom alert path itself dies. |
| Auto-recovery | Pool watchdog reconnects with `SubscribeRxGuard` |
| Operator action | If Dhan-side, wait. If static IP, verify `aws ec2 describe-addresses` + Dhan `/v2/ip/getIP`. |
| Runbook | EXISTING: `docs/runbooks/dhan-api-down.md`, `docs/runbooks/websocket-depth.md` |

### Case J — Tickvault OOM-killed mid-boot

| Field | Value |
|---|---|
| Symptom | App was emitting logs, then suddenly silent. EC2 still running. |
| Root cause | Boot-time peak memory exceeded t4g.medium 8GB envelope (e.g., rkyv cache deserialization spike) |
| Detection | Linux OOM-killer SIGKILLs PID. systemd notes Result=signal SIGKILL → restart. After 3 SIGKILLs → halt → boot-not-complete alarm |
| Routing | Out-of-band (in-band silenced — process is dead) |
| Auto-recovery | systemd 3-retry. Recovery only if the OOM was transient. |
| Operator action | SSH; `dmesg | grep -i kill`; check `aws-budget.md` rule 11 (2GB headroom floor); if memory budget exceeded → bigger instance OR trim app footprint. |
| Runbook | NEW STUB: `docs/runbooks/aws-oom-during-boot.md` |

### Case K — Tickvault crashes during market hours (post-boot)

| Field | Value |
|---|---|
| Symptom | Was healthy at 09:15. At 11:23 it died. |
| Root cause | Panic in hot path, OOM, or systemd-killed |
| Detection | App emitted in-band CRITICAL before death (best effort). Plus: `tv_app_alive_metric` gauge stops updating → CW alarm "metric missing 60s, TreatMissingData=breaching" → SNS |
| Routing | In-band (last gasp) + Out-of-band primary |
| Auto-recovery | systemd `Restart=on-failure RestartSec=3`. Boot replays from Mode C (mid-market). RTO ≈ 30s per `disaster-recovery.md` Scenario 1. |
| Operator action | If restart succeeds → review logs; if 3 restarts fail → manual intervention. |
| Runbook | EXISTING: `docs/runbooks/aws-disaster-recovery.md` §4 |

### Case L — Operator is asleep / phone in DND

| Field | Value |
|---|---|
| Symptom | Any of the above fires, but the human doesn't see for hours |
| Root cause | Human factor. Cannot be fixed by software alone. |
| Detection | If all 3 SNS legs fired and no human acknowledged within 10 minutes → escalation Lambda triggers SECOND alarm to a backup channel (e.g., AWS Chatbot in Slack, or phone CALL via Amazon Connect / Twilio) |
| Routing | Escalation tier: SNS topic `tv-prod-alerts-escalation` with 10-min `notification-acknowledged` Lambda gate |
| Auto-recovery | The trade-day's "I missed it" is unrecoverable; kill switch must auto-engage if app cannot resume by 09:14:30 IST (Phase2 readiness gate) |
| Operator action | Acknowledge alert via simple `aws sns publish --topic tv-prod-alerts-ack` from phone OR a one-tap webhook button OR replying to the Telegram bot |
| Runbook | NEW STUB: `docs/runbooks/aws-operator-no-response.md` |

**Acceptance criterion:** "operator-unaware" window ≤ 5 minutes for soft failures, ≤ 15 minutes for hard failures (per operator demand in topic).

---

## §4. The 100% coverage promise — HONEST envelope

Per operator-charter §F, the literal-100% claim is rewritten as:

> "Inside the tested envelope, EVERY one of the 12 worst cases in §3 is detected by an AWS-side primitive AND routed to at least one of {SMS, Email, Telegram-webhook} within 60 seconds of the failure becoming observable to AWS. Beyond the envelope (region-wide AWS outage, all 3 SNS legs failing simultaneously, operator phone destroyed), the contract reduces to: at next operator session-start, `mcp__tickvault-logs__run_doctor` + `make doctor` surface the missing-boot state within 10 seconds."

The envelope explicitly does NOT cover:
- Region-wide AWS ap-south-1 outage (deferred to §6 cross-region option)
- Simultaneous failure of all 3 SNS legs + operator's secondary device
- A human who chooses to ignore alerts

---

## §5. Detection latency budget (per worst-case)

| Case | Detection primitive | Latency target | How measured |
|---|---|---|---|
| A — Rule disabled | Missing-invocation alarm | ≤7 min (alarm needs 1 period to confirm absence) | `EventBridge.Invocations` metric |
| B — IAM/throttle | `Scheduler.TargetErrorCount` | ≤2 min | CW metric publication interval |
| C — Capacity | SSM Automation `Failed` state | ≤2 min | SSM state-change event |
| D — cloud-init | "boot-not-complete by T+180s" alarm | ≤60s after deadline | Custom metric `tv_boot_complete` |
| E — docker dead | systemd 3-retry exhaust | ≤90s | Same custom metric |
| F — binary fail | CW Logs filter on systemd exit | ≤30s | CW Logs delivery |
| G — Dhan auth | In-band Telegram, ECS Logs filter backup | ≤10s | Direct emit |
| H — QuestDB | In-band `BOOT-01` then `BOOT-02` | ≤60s | App log |
| I — WS connect | In-band + Prom alert | ≤60s | Prom evaluation |
| J — OOM boot | "boot-not-complete" alarm | ≤60s | Same as D |
| K — Crash mid-day | `tv_app_alive_metric` missing | ≤60s | CW alarm `TreatMissingData=breaching` |
| L — Operator AWOL | Ack-not-received-in-10-min | ≤10 min | Lambda + state |

---

## §6. Self-healing vs alert-and-wait decisions

| Case | Auto-recovery attempted? | Why |
|---|---|---|
| A | No | EventBridge cannot heal itself |
| B | Yes (3 retries) | Transient IAM/throttle resolves on retry |
| C | No | AZ capacity is AWS-side; we'd need multi-AZ design |
| D | systemd restart only | user-data is first-boot; re-run requires AMI rebuild |
| E | Yes (`systemctl restart docker` in user-data wrapper) | Docker daemon crashes are usually transient |
| F | Yes (3 systemd retries) | Bad config persists; binary corruption persists |
| G | Yes (token manager 3 attempts) | Token cache → SSM → TOTP cascade |
| H | Yes (BOOT-01 60s wait, then HALT) | QuestDB usually recovers within 30s |
| I | Yes (pool watchdog reconnect) | TCP transient |
| J | systemd retry only | OOM tends to recur; needs config change |
| K | Yes (systemd retry + Mode C boot) | Mid-market crash recoverable in ~30s |
| L | No | Human must acknowledge |

---

## §7. Cross-channel redundancy diagram (insta-reel format)

```
            08:30 IST — the moment of truth
                       │
                       ▼
        ┌──────────────────────────────────┐
        │   Does EventBridge rule fire?    │
        └──────┬───────────────────┬───────┘
               │ YES               │ NO
               ▼                   ▼
   ┌────────────────────┐  ┌───────────────────────┐
   │ Did EC2 reach      │  │ "Missing invocation"  │
   │ running state?     │  │ CW alarm @ T+7min     │
   └──┬─────────────┬───┘  └───────────┬───────────┘
      │ YES         │ NO              ▼
      ▼             ▼                SNS
   ┌─────────┐  Capacity/IAM       (SMS+Email+TG)
   │ App     │  alarm @ T+2min          │
   │ boots?  │     │                    ▼
   └──┬──┬───┘     ▼              Operator sees @ T+8min
      │  │        SNS
      │  │ NO
      │  ▼
      │ "tv_boot_complete missing" alarm @ T+180s+60s
      │     │
      │     ▼
      │   SNS (SMS+Email+TG)
      │     │
      │     ▼
      │ Operator sees @ T+4min
      │
      ▼ YES
   ┌──────────────────────────┐
   │ In-band Telegram         │
   │ "✅ tickvault started"   │ ← operator sees @ T+3min (happy)
   └──────────────────────────┘
```

**Hierarchy:** in-band wins on speed (T+3min), out-of-band wins on resilience (T+4 to T+8min even if app dead).

---

## §8. The Z+ 7-layer defense for the lifecycle

Per `z-plus-defense-doctrine.md` — every component must answer the 7-layer test.

| Layer | For EventBridge cron | For the EC2 boot | For app boot completion |
|---|---|---|---|
| L1 DETECT | Invocation metric polled @1min | EC2 state-change event @<1min | Custom metric scrape @60s |
| L2 VERIFY | DLQ depth alarm | Status check 2/2 confirmation | App-emitted `BootReadyConfirmation` Telegram |
| L3 RECONCILE | Daily 08:25 IST "pre-fire" Lambda confirms rule + IAM still exist | Daily 08:45 IST self-test (existing `SelfTestPassed`) | Daily `previous_close` table row count check (`operator-daily-startup.md` §E) |
| L4 PREVENT | IAM policy review automation (cfn drift detect) | AMI hardening — pre-install CW agent, pre-vet docker | Boot gate refuses to start if SSM TOTP secret hash changed (AUTH-GAP-04) |
| L5 AUDIT | CloudTrail `StartInstances` event | EC2 `boot_audit` table (Wave-2-D) | `selftest_audit` table |
| L6 RECOVER | Manual re-enable + manual start | systemd 3-retry + AMI rebuild | Operator manual via runbook |
| L7 COOLDOWN | 5-min rate limit on alarm re-fire | 30s wait between systemd retries | Coalesced Telegram per `coalescer.rs` |

---

## §9. What's MISSING from the repo today (gap list — operator decision needed)

| # | Gap | Effort | Blocking? |
|---|---|---|---|
| 1 | EventBridge `RetryPolicy` + DLQ on the 4 rules | 30 LoC Terraform | YES — case B/C blind without |
| 2 | CW alarm `MissingInvocation` for each rule | 60 LoC Terraform | YES — case A blind without |
| 3 | CW alarm `tv_boot_complete missing` | 40 LoC Terraform + app emits 1 gauge | YES — cases D/E/J blind without |
| 4 | CW Logs subscription filter `systemd exit nonzero` → SNS | 50 LoC Terraform | YES — case F slow without |
| 5 | SNS HTTPS subscription to Telegram bot webhook | 100 LoC Terraform + 1 small Lambda or API Gateway | YES — out-of-band Telegram leg unwired |
| 6 | SNS SMS subscription to operator phone (DLT registered) | DLT registration paperwork + 5 LoC Terraform | YES |
| 7 | SNS Email subscription | 5 LoC Terraform | YES — trivial |
| 8 | Pre-baked AMI with CW agent tailing `/var/log/cloud-init-output.log` | 1 packer build pipeline | Optional — high value |
| 9 | Escalation Lambda for "operator AWOL" (case L) | ~150 LoC Python | Optional |
| 10 | Cross-region SNS for ap-south-1 outage envelope | 80 LoC Terraform | Optional — accepts envelope |
| 11 | `tv_app_alive_metric` gauge emitted every 30s by app | 20 LoC Rust + gauge declaration | YES — case K blind without |
| 12 | Schedule lock — Terraform says 08:00 IST; aws-budget.md says 08:30 IST. Operator must choose. | 4 lines Terraform | YES |

**Total estimated effort:** 1–2 weeks of focused work (1 Terraform PR, 1 small Lambda PR, 1 Rust gauge PR, AMI rebuild). All gated behind one plan: `.claude/plans/aws-lifecycle/active-plan-aws-lifecycle.md`.

---

## §A. Schedule — LOCKED 2026-05-18 to 08:00 / 17:00 IST

**Operator lock 2026-05-18:** 08:00 IST start, 17:00 IST stop weekdays. 08:00 IST start, 13:00 IST stop weekends. Terraform `main.tf:253-275` wins; `aws-budget.md` rule 8 MUST be updated to match (4-line edit; gated behind Item AWS-1).

### Historical drift note (pre-2026-05-18)

`deploy/aws/terraform/main.tf:247-250` says:

```
Weekday start 08:00 IST = 02:30 UTC (Mon-Fri)
Weekday stop  17:00 IST = 11:30 UTC (Mon-Fri)
Weekend start 08:00 IST = 02:30 UTC (Sat-Sun)
Weekend stop  13:00 IST = 07:30 UTC (Sat-Sun)
```

`.claude/rules/project/aws-budget.md` rule 8 says:

```
Weekday Start: 8:30 AM IST  (cron(0 3 ? * MON-FRI *))
Weekday Stop:  5:30 PM IST  (cron(0 12 ? * MON-FRI *))
Weekend Start: 8:30 AM IST  (cron(0 3 ? * SAT,SUN *))
Weekend Stop:  1:30 PM IST  (cron(0 8 ? * SAT,SUN *))
```

These are inconsistent. The 30-min difference is operationally meaningful because:
- Phase 2 fires at **09:13 IST** (per `subscription_planner.rs`). 08:00 IST gives 73 min runway; 08:30 IST gives 43 min runway.
- 08:30 IST is the documented Operator Daily Startup time (`operator-daily-startup.md` line 1).

**Operator decided 08:00 IST.** 73-min runway to 09:13 IST Phase 2 gate is generous; the 30-min cost premium (~₹170/mo) is accepted. `operator-daily-startup.md` time anchors will be re-anchored from 08:30 → 08:00 in a separate runbook PR.

---

## §B. Cost reality check — LOCK-2 PENDING HONEST RECONCILIATION

**Operator lock 2026-05-18:** target <₹1K/mo, "accept compromises TBD."

**Honest finding:** <₹1K/mo is NOT structurally achievable on AWS with the current architecture (Dhan static-IP mandate + 8GB host memory budget + 5-year SEBI retention + 24/7 EIP). See `docs/architecture/aws-cost-floor-analysis.md` for the math. The realistic AWS-only floor is ~₹1.4K/mo. Sub-₹1K requires either off-AWS hosting or relaxing one of the architectural invariants. **OPERATOR DECISION OUT OF SCOPE OF THIS DOC** — see the cost-floor doc.

### Historical cost note (pre-2026-05-18)

`aws-budget.md` shows the **₹4,981/mo budget** with t4g.medium + EBS + S3 + EIP + CloudWatch free tier + SNS + Data Transfer. The operator's prior-session claim of "<₹1K" is NOT in repo and would require:

| Change | Saves | New monthly cost |
|---|---|---|
| Downsize t4g.medium → c7i.large | ₹1,765 | ~₹3,200 |
| Skip Saturday | ₹600 | ~₹4,400 |
| Skip Saturday + Sunday | ₹1,200 | ~₹3,800 |
| Spot instances (operator REJECTED earlier per aws-budget.md rule "on-demand only") | up to ₹2,000 | not in scope |

**₹1K/mo is NOT achievable without architectural compromise.** Operator must reconfirm or update `aws-budget.md`.

---

## §C. References (research-anchored)

- AWS EventBridge Scheduler retry policy: https://docs.aws.amazon.com/scheduler/latest/APIReference/API_RetryPolicy.html
- AWS Scheduler CloudWatch metrics: https://docs.aws.amazon.com/scheduler/latest/UserGuide/monitoring-cloudwatch.html
- EC2 InsufficientInstanceCapacity: https://repost.aws/knowledge-center/ec2-insufficient-capacity-errors
- SNS SMS pricing India (DLT route ~$0.00278): https://aws.amazon.com/sns/sms-pricing/
- SNS retry policy (50 attempts, ≤3600s): https://docs.aws.amazon.com/sns/latest/dg/sns-message-delivery-retries.html
- CW Alarm → SNS direct (sub-second): https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/Notify_Users_Alarm_Changes.html
- cloud-init log visibility: https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/EC2NewInstanceCWL.html
- Telegram Bot rate limits: https://core.telegram.org/bots/faq
- Existing repo anchors: `deploy/aws/terraform/main.tf:253-275`, `deploy/aws/terraform/alarms.tf`, `crates/core/src/notification/service.rs:50-89`, `deploy/aws/lambda/claude-triage/handler.py`

> (2026-07-18: `deploy/aws/lambda/claude-triage/handler.py` was deleted in the Rust-only purge — retrieve it via git history at any pre-2026-07-18 commit.)

---

## §D. 9-box per-item checklist (mandatory per operator-charter §C)

This design doc itself is the discussion artefact; each implementation item in `.claude/plans/aws-lifecycle/active-plan-aws-lifecycle.md` will carry the 9-box + 15-row + 7-row matrix individually.

---

## §E. Honest 100% claim (mandatory per operator-charter §F)

> "100% inside the tested envelope: every one of the 12 worst-case scenarios in §3 is detected by an AWS-native primitive within the latency budget in §5 AND routed via at least one of 3 independent SNS legs (SMS / Email / Telegram-webhook) to the operator's device within 60 seconds of detection. Envelope bounds: ap-south-1 region is up; at least 1 SNS leg's downstream provider (carrier SMS / mail / Telegram) accepts the delivery; operator device is powered and has network. Beyond the envelope, the doctor session-start hook surfaces missing state at next interactive session within 10 seconds. **NOT promised:** sub-60s end-to-end delivery (no AWS service publishes such a numeric SLA), survival of a full ap-south-1 SNS outage without cross-region fallback, or human acknowledgement within any specific window."
