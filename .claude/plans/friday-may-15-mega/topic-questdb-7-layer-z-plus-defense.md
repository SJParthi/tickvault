# Topic — QuestDB 7-Layer Z+ Defense (Charter Line #3)

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `z-plus-defense-doctrine.md` > this file.
> **Scope:** Apply the 7-layer Z+ defense pattern to the QuestDB persistence stack. Operator's charter: "QuestDB should never ever fail or always disconnect."
> **Honest envelope:** Cannot promise literal "never fails" — QuestDB is a remote Docker process. Promise = bounded recovery inside chaos envelope.

---

## 🚗 Auto-Driver Story

> Sir, our QuestDB is like the central bank vault. Every tick, every order, every audit row goes into it. If the vault locks itself shut, our entire trading day is lost. So we put 7 layers of bodyguards around the vault — TIRE patrols (L1), GATE checkers (L2), DAILY inspectors (L3), PRE-ARRIVAL screeners (L4), CAMERAS (L5), an EMERGENCY EVACUATION plan (L6), and a COOLING-OFF rule that says "don't ram the gate if it's already stuck — wait 30 sec first" (L7).
>
> Today we discuss what each of those 7 looks like FOR THE VAULT.

---

## 📋 The 11 Root Causes of "QuestDB Failed"

| # | Root cause | Likelihood | Severity |
|---|---|---|---|
| 1 | ILP TCP connection drop (network blip) | HIGH | Medium |
| 2 | Schema drift — column missing in table | MEDIUM | High |
| 3 | Disk full (gp3 100GB cap) | LOW | Critical |
| 4 | Memory pressure → Docker OOM kill | LOW | Critical |
| 5 | WAL replay slow on container restart | MEDIUM | High |
| 6 | Partition manager dropping wrong partition | LOW | Critical |
| 7 | DEDUP key collision (composite key wrong) | LOW | High |
| 8 | Slow query starving ILP writes | MEDIUM | High |
| 9 | Container crash mid-write | LOW | Critical |
| 10 | Lock file orphaned after `kill -9` | LOW | Medium |
| 11 | QuestDB version upgrade with breaking schema change | RARE | Critical |

---

## 🛡️ The 7-Layer Z+ Defense for QuestDB

### L1 DETECT — Fast sensors, always-on

| Sensor | What it watches | Threshold |
|---|---|---|
| `tv_questdb_ilp_write_errors_total` | Per-table ILP append errors | rate > 0.1/sec for 30s |
| `tv_questdb_ilp_flush_duration_ns` | p99 flush latency | p99 > 100ms for 5min |
| `tv_questdb_disconnected_seconds` | Time since last successful write | > 30s |
| `tv_questdb_disk_free_pct` | Disk free percentage | < 20% |
| `tv_questdb_container_restart_total` | Docker restart counter | > 0 in last hour |
| `tv_questdb_select_query_duration_ns` | p99 SELECT latency | p99 > 5sec for 5min |
| `tv_questdb_rescue_ring_depth` | Ticks queued in rescue ring | > 50% capacity |

**Total NEW metrics:** 7. **NEW alert rules:** 7.

### L2 VERIFY — Cross-check before alarming

After every ILP flush, do we re-SELECT to verify the row landed?

| Option | Description | Cost |
|---|---|---|
| (a) Verify EVERY flush | Re-SELECT after every batch | ~1ms × 1000/sec = 1 sec/sec CPU = NO |
| (b) Sample 1% of flushes | Re-SELECT 1% randomly | ~10ms overhead/sec = acceptable |
| (c) Verify only on flush ERROR | Re-SELECT only when flush returns error | minimal overhead, but blind to silent corruption |
| (d) Verify CRITICAL tables only | Re-SELECT only `order_audit` + `phase2_audit` | best ROI |

**My vote:** Option (d) for critical tables, Option (b) sampling for high-volume tables (ticks, candles_*).

### L3 RECONCILE — Daily ground-truth sync

| Reconcile | Source | Target | Action on drift |
|---|---|---|---|
| Tick count | App-side counter | `SELECT count(*) FROM ticks WHERE date = today` | Alert if delta > 1% |
| Candle count | Aggregator emission count | `SELECT count(*) FROM candles_1m WHERE date = today` | Alert if delta > 0.1% |
| Order count | OMS state | `SELECT count(*) FROM order_audit WHERE date = today` | HALT if any mismatch |
| Schema | Migrations log | `SHOW TABLES` + `SHOW COLUMNS` | Alert on drift |
| Disk usage | App estimate | Docker `df` | Alert if delta > 10% |

**When:** Daily 23:00 IST. Why: after market close, before partition manager runs.

### L4 PREVENT — Stop the failure before it happens

| Pre-flight | When | What it checks |
|---|---|---|
| Disk-free pre-flight | At boot + every 5min | `df -h /data` > 20% free |
| Schema self-heal | At boot | `CREATE TABLE IF NOT EXISTS` + `ALTER ADD COLUMN IF NOT EXISTS` |
| QuestDB readiness probe | At boot | `wait_for_questdb_ready(60s)` per BOOT-01/02 |
| ILP TCP socket pre-warm | At boot | Open + ping ILP port before first tick |
| Partition manager dry-run | Pre-archive | Verify partition age > retention before dropping |
| DEDUP key compile-time check | At build | Macro asserts key includes segment |

### L5 AUDIT — Continuous observability

Existing audit tables (per Wave-2-D):
- `boot_audit`
- `phase2_audit`
- `ws_reconnect_audit`
- `selftest_audit`
- `depth_rebalance_audit`
- `order_audit` (SEBI 5y)

**NEW audit tables for Z+:**
- `questdb_health_audit` — per-flush latency, error counter, disconnect events
- `partition_lifecycle_audit` — every partition create/detach/archive logged
- `schema_change_audit` — every `ALTER` operation logged

### L6 RECOVER — Nuclear option

Existing (Wave-2-D + audit-findings):
- **Tier 1:** Rescue ring (5M ticks) absorbs short outages
- **Tier 2:** NDJSON spill files when ring fills
- **Tier 3:** DLQ catches anything spill misses

**NEW for Z+:**
- **Tier 4:** Auto-restart QuestDB container if `tv_questdb_disconnected_seconds > 120s`
- **Tier 5:** If 3 container restarts in 1 hour → escalate to HALT + Telegram CRITICAL
- **Tier 6:** Operator manual intervention (run `make doctor` + check Docker logs)

### L7 COOLDOWN — Don't let recovery cause damage

| Cooldown | Duration | Why |
|---|---|---|
| Between ILP reconnect attempts | 5s exponential backoff | Avoid TCP SYN flood |
| Between container restarts | 60s | Let WAL replay complete |
| Between partition manager runs | 24h | Idempotent but disk-IO heavy |
| Between schema migrations | At boot only | Avoid concurrent ALTER conflicts |

---

## 📊 The 11-cell coverage matrix

| Root cause | L1 catches | L2 catches | L3 catches | L4 catches | L5 catches | L6 catches | L7 catches |
|---|---|---|---|---|---|---|---|
| 1 ILP TCP drop | ✅ disconnect gauge | ✅ resub on reconnect | ✅ daily count reconcile | — | ✅ health audit | ✅ rescue ring | ✅ 5s backoff |
| 2 Schema drift | — | ✅ verify SELECT fails | ✅ schema reconcile | ✅ self-heal at boot | ✅ schema_change_audit | ✅ alter add column | — |
| 3 Disk full | ✅ disk-free gauge | — | ✅ disk-usage reconcile | ✅ pre-flight 5min | ✅ partition_lifecycle | ✅ partition manager drops old | — |
| 4 OOM kill | ✅ restart counter | — | — | ✅ memory pre-flight | ✅ boot_audit | ✅ tier 4 auto-restart | ✅ 60s between restarts |
| 5 WAL replay slow | ✅ disconnect gauge | — | — | ✅ readiness probe BOOT-01/02 | ✅ boot_audit | ✅ wait for ready | ✅ 60s backoff |
| 6 Wrong partition dropped | — | ✅ daily count reconcile | ✅ count delta alert | ✅ dry-run before drop | ✅ partition_lifecycle | ❌ NO RECOVERY (data lost) | — |
| 7 DEDUP key wrong | — | ✅ verify SELECT shows duplicates | ✅ count reconcile | ✅ compile-time meta-guard | ✅ schema_change_audit | — | — |
| 8 Slow query | ✅ p99 latency gauge | — | — | — | ✅ slow query audit | — | — |
| 9 Container crash | ✅ restart counter | — | — | — | ✅ boot_audit | ✅ rescue ring + tier 4 | ✅ 60s |
| 10 Lock file orphaned | ✅ readiness probe fails | — | — | ✅ boot script clears stale lock | ✅ boot_audit | ✅ manual operator | — |
| 11 Version upgrade breaking | — | — | ✅ schema reconcile | ✅ alter add column | ✅ schema_change_audit | ❌ MANUAL ROLLBACK | — |

**Coverage gaps (red flags):**
- Root cause #6 (wrong partition dropped) has NO L6 recovery. Data loss is permanent.
- Root cause #11 (version upgrade) requires manual rollback. NO auto recovery.

---

## 🚨 The 2 worst-case nightmares

### Nightmare 1 — Partition manager drops TODAY's partition

A bug in the partition manager calculates `partition_date < retention_days_ago` incorrectly. Today's partition has age = 0; off-by-one bug treats `0 < retention_days_ago` as true → drops today.

**Result:** Today's ticks + candles + audits all gone. Mid-session. Trading on stale data possible.

**Defense:**
- L4 PREVENT: dry-run mode logs intended drops; operator reviews
- L4 PREVENT: `static_assert!(retention_days >= 1)` at compile time
- L4 PREVENT: partition manager refuses to drop ANY partition with `date >= today - 30 days`
- L5 AUDIT: every drop attempt logged with `intent` + `actual` + `partition_date`
- L6 RECOVER: weekly S3 snapshot — can restore previous day's partition

### Nightmare 2 — QuestDB version upgrade silently changes DEDUP semantics

QuestDB 9.4 vs 9.3 — DEDUP UPSERT KEYS behavior change. New version accepts our key but applies it differently. Result: duplicate rows accumulate; query results wrong.

**Defense:**
- L4 PREVENT: pin QuestDB Docker image with SHA256 digest (already done)
- L4 PREVENT: upgrade requires explicit operator approval + 24h soak in dev
- L3 RECONCILE: daily DEDUP integrity check — `SELECT count(*), count(distinct dedup_key) FROM <table>`
- L5 AUDIT: schema_change_audit captures every version bump
- L6 RECOVER: rollback Docker image; replay from spill

---

## 🎯 Discussion items

### D1 — Auto-restart QuestDB container — yes or no?

**Pro:** No operator action needed. Self-healing for 80% of failures.

**Con:** Restart loop if root cause is config bug. Data lock during 60s restart window.

**Counter-pro:** L7 cooldown (60s) + max 3 restarts/hour prevents storm. After 3, HALT + Telegram.

**Counter-con:** What if the container needs `docker-compose down && up` (not just `restart`)?

**My vote:** YES auto-restart with strict cap. NO if cap exceeded → HALT.

### D2 — How aggressive should L1 detection be?

Current proposal: `tv_questdb_disconnected_seconds > 30s` → alert.

**Alternative:** 10s alert (more sensitive but more false positives during routine ILP reconnect).

**My vote:** 30s. Per existing `resilience_sla_alert_guard.rs`.

### D3 — Partition manager safety — how paranoid?

Three levels:

| Level | Behavior | Risk |
|---|---|---|
| (a) AUTO drop everything > retention_days | Fully automated | Off-by-one bug = data loss |
| (b) DRY-RUN report → operator approves daily | Semi-automated | Operator dependency |
| (c) NEVER auto-drop. S3 archive only. | Conservative | Disk fills faster |

**My vote:** (b) for first 90 days post-deploy, then (a) once trust established.

### D4 — How to test "QuestDB down" in CI?

Chaos test exists: `chaos_questdb_docker_pause.rs`. But it pauses for 60s — does that cover all 11 root causes?

**Proposal:** Add 4 more chaos tests:
- `chaos_questdb_disk_full.rs`
- `chaos_questdb_schema_drift.rs`
- `chaos_questdb_oom_kill.rs`
- `chaos_questdb_partition_drop_wrong.rs`

Total chaos suite: 16 → 20.

### D5 — SEBI audit retention conflict

Order_audit needs 5-year retention. QuestDB 90d hot → S3 IT (90-365d) → Glacier Deep Archive (≥1y).

**Question:** What if S3 archive fails for 3 days? After 90d, partition manager wants to drop. Drop = SEBI violation.

**Proposal:** Partition manager REFUSES to drop if `s3_archive_audit.outcome != Success` for that partition. Forces operator to fix S3 first.

---

## 🎤 The OPERATOR question

Operator's charter says "QuestDB never fails". My honest envelope says "60s outage absorbed; beyond that, DLQ NDJSON catches every payload."

**Is that ENOUGH for operator's trust?**

Three escalations possible:
- **More:** Bound the envelope tighter — 30s instead of 60s. Costs: bigger rescue ring (10M ticks).
- **Same:** Keep 60s. Trust the 3-tier rescue → spill → DLQ.
- **Less:** Loosen to 120s. Costs: more operator visibility burden but allows slower L6 recovery.

**My recommendation:** SAME (60s) with the NEW L1-L7 additions on top. The existing envelope is already chaos-tested.

---

## 🎤 Open for argument

Floor's yours. D1-D5 or new angle.
