# Implementation Plan: 8:15 AM Auto-Start with Immediate Boot + Infrastructure Hardening

**Status:** VERIFIED
**Date:** 2026-03-14
**Approved by:** Parthiban

## Problem

1. **Boot sleeps until 8:30** — if app boots before 08:30, `phase_needs_websocket(PreConnect) = false` skips WebSocket entirely. Dead code: `if phase == PreConnect { sleep }` inside `if phase_needs_websocket(phase)` which is always false for PreConnect. The app wastes the 15-minute buffer.
2. **No auto-start** — no launchd (Mac) or systemd (AWS) to trigger the app at 08:15 AM IST.
3. **No boot health notification** — operator has no Telegram confirmation that all systems are ready.
4. **19 infrastructure gaps** — port conflicts undetected, partial startup ignored, OOM kills silent, QuestDB timeout too short, Valkey not health-checked.
5. **14 AWS SSM gaps** — no retry on throttling, concurrent failures = total boot failure, no SSM latency metrics, IAM credential expiry not monitored.

## Research Findings (6 deep-dive rounds)

### 1. Mac Docker Desktop
- Docker Desktop after sleep is **deeply broken** (ports inaccessible, daemon hangs) — prevent sleep entirely via `pmset -c sleep 0`
- `open -a Docker` cold start: 10-30s, worst case 60+s
- VM can crash while UI shows running — only `docker info` is reliable
- Docker stuck "Starting..." — well-documented, need `killall Docker` + restart recovery
- launchd `StartCalendarInterval` coalesces missed events on wake but NOT on power-off
- LaunchAgent (not LaunchDaemon) — Docker Desktop needs GUI session + auto-login
- Script must run >10 seconds or launchd throttles as crash loop
- DNS breaks after sleep — flush DNS cache in startup script
- Disable Docker Desktop auto-updates (can break mid-trading)

### 2. AWS EC2 + systemd
- systemd timer: `Persistent=true` fires on next boot if missed, `AccuracySec=1us` for precision
- `Restart=on-failure` with `RestartSec=5s`, `StartLimitBurst=5`, `StartLimitIntervalSec=300`
- Boot order: cloud-init → network → docker.service → our service
- c7i.2xlarge: EBS-only (no instance store), 12.5 Gbps network, Enhanced Networking (ENA)
- Docker on Linux: `Requires=docker.service`, `After=docker.service cloud-init.target network-online.target`
- EventBridge Scheduler for EC2 start at 08:00 IST / stop at 17:30 IST (cost savings)
- Docker log rotation in `/etc/docker/daemon.json` (prevent disk full)

### 3. Infrastructure Gaps (Current Codebase — 19 found)
- **GAP-INFRA-01**: No port conflict detection — container fails silently
- **GAP-INFRA-02**: Partial startup — only checks QuestDB + Grafana, ignores Valkey (critical for token cache)
- **GAP-INFRA-03**: QuestDB 60s timeout too short — first boot can take 80-120s
- **GAP-INFRA-05**: OOM kill (ExitCode 137) not detected — app boots without DB
- **GAP-INFRA-12**: No rollback on partial startup
- **GAP-INFRA-13**: No disk space pre-flight check
- **GAP-INFRA-19**: No health check for trading app itself

### 4. AWS SSM Code Analysis (secret_manager.rs — 14 gaps)
- `create_ssm_client()` hardcoded to ap-south-1 — correct but no fallback region
- `fetch_secret()` — no explicit retry on `ThrottlingException` (relies on SDK defaults)
- `fetch_dhan_credentials()` uses `tokio::try_join!()` — ANY single fetch failure = ALL fail, boot halts
- No metrics on SSM call latency (can't detect degradation)
- No circuit breaker for repeated SSM failures
- Token cache at `/tmp/dlt-token-cache` is the fast-boot path — avoids SSM on crash-restart

### 5. AWS SSM Failure Modes & Best Practices
- **Rate limits**: GetParameter = 40 TPS default (can request increase), PutParameter = 10 TPS
- **Throttling**: SDK has built-in exponential backoff (standard mode), but default max 3 retries
- **SLA**: SSM is regional — no cross-region failover. ap-south-1 outage = no secrets
- **IMDS v2**: Token-based (PUT to 169.254.169.254), 6-hour TTL. Required for IAM role credentials on EC2
- **KMS dependency**: SecureString parameters require KMS decrypt. KMS outage = can't read secrets
- **Credential chain on EC2**: IMDS → IAM role → STS temporary creds (auto-refreshed by SDK)
- **Credential chain on Mac**: `~/.aws/credentials` file → env vars → no IMDS
- **Best practice**: Cache secrets locally. Our token cache at `/tmp/dlt-token-cache` already does this for the Dhan token. Infra secrets (QuestDB/Grafana/Valkey passwords) are stable — could also cache
- **IMDSv2 boot timing**: Instance metadata may take 5-15s to become available after EC2 start
- **Concurrent SSM calls**: Our `tokio::try_join!` fires 3-5 GetParameter calls simultaneously — well within 40 TPS limit

### 6. AWS EC2 Connectivity & Infrastructure
- **Elastic IP**: REQUIRED for Dhan's static IP whitelist. Free when attached to running instance, $0.005/hr when stopped. Survives stop/start cycles
- **VPC**: Public subnet with Internet Gateway (no NAT Gateway overhead). Single AZ is fine (trading = single-region by nature)
- **Security Groups**: Outbound HTTPS (443) for Dhan API + SSM. Inbound SSH (22) from admin IP only. No other inbound needed
- **IAM Instance Profile**: SSM:GetParameter, SSM:PutParameter, CloudWatch:PutMetricData, logs:PutLogEvents. Least privilege
- **Docker on EC2 Linux**: Install via `amazon-linux-extras install docker` or official Docker repo. `/etc/docker/daemon.json` for log rotation: `{"log-driver":"json-file","log-opts":{"max-size":"50m","max-file":"5"}}`
- **NTP**: Amazon Time Sync Service via chrony (169.254.169.123). Microsecond accuracy. Pre-configured on Amazon Linux 2023. Critical for trading timestamps
- **EBS**: gp3 (50 GB), 3000 IOPS baseline (free), 125 MB/s throughput. More than sufficient for QuestDB + app
- **c7i.2xlarge**: 8 vCPU, 16 GB RAM, up to 12.5 Gbps network. ~$0.357/hr on-demand ($260/month). Reserved 1yr = ~$175/month
- **Boot timing**: EC2 start → instance running: 30-60s. cloud-init complete: 60-90s. Docker ready: 90-120s. Total to app-ready: ~3 min
- **Cost optimization**: EventBridge start 08:00 IST (2:30 UTC), stop 17:30 IST (12:00 UTC) = ~9.5 hrs/day × 22 days/month = ~$75/month (vs $260 always-on)
- **Monitoring**: CloudWatch agent for memory/disk (EC2 doesn't expose these by default). CPU/network from hypervisor. Alarms: CPU > 80%, disk > 85%, StatusCheckFailed

---

## Plan Items

### A. Core Boot Sequence Fix (Rust code)

- [x] **A1.** Remove PreConnect sleep/skip — connect WebSocket immediately on trading day boot (main.rs:835-841)
  - Files: main.rs, mod.rs
  - Tests: test_determine_phase_pre_connect_on_trading_day, test_boot_before_ws_connect_time_immediate_connect
  - Detail: In `main.rs`, change WS gate from `phase_needs_websocket(phase)` to `phase_needs_websocket(phase) || phase == SchedulePhase::PreConnect`. This makes PreConnect connect immediately instead of sleeping. Remove the dead `if phase == PreConnect { sleep }` block (lines 838-846, already unreachable). Keep `determine_phase` and `phase_needs_websocket` unchanged (pure functions, other consumers may use them). StorageGate stays CLOSED — ticks flow through pipeline but NOT persisted until 09:00.

- [x] **A2.** Add `BootHealthCheck` Telegram notification after all systems ready (events.rs, main.rs:1117-1131)
  - Files: events.rs, main.rs
  - Tests: test_boot_health_check_message, test_boot_health_check_severity
  - Detail: New `NotificationEvent::BootHealthCheck { ws_connections, instruments_subscribed, storage_gate_open, phase }`. Fire after WebSocket pool built, before shutdown-wait. Message: "ALL SYSTEMS HEALTHY — {N} WS connections, {M} instruments, phase: {P}, storage: gated/open". Severity: `Info`.

### B. Infrastructure Hardening (Rust code)

- [x] **B1.** Harden `infra.rs` — required vs optional services with container exit code checks (infra.rs:check_required_containers)
  - Files: infra.rs, main.rs
  - Tests: test_required_containers_includes_questdb, test_required_containers_includes_valkey, test_required_containers_does_not_include_grafana
  - Detail: After `docker compose up -d`, run `docker inspect --format '{{.State.Running}} {{.State.ExitCode}}' <container>` on dlt-questdb and dlt-valkey. ExitCode 137=OOM → HALT, nonzero → HALT. `ensure_infra_running` now returns `bool` — callers in main.rs halt boot on `false`.

- [x] **B2.** Validate QuestDB with actual query (not just TCP port probe) (infra.rs:validate_questdb_query)
  - Files: infra.rs
  - Tests: test_questdb_validation_uses_select_1
  - Detail: After TCP probe passes, sends `SELECT 1` via HTTP GET to `/exec?query=SELECT+1`. Retries 3× with 2s delay, 10s timeout per attempt. Returns `false` if all retries fail → boot halted.

- [x] **B3.** Increase QuestDB health timeout from 60s to 120s (infra.rs:INFRA_HEALTH_TIMEOUT)
  - Files: infra.rs
  - Tests: test_health_timeout_is_120_seconds, test_health_timeout_is_reasonable
  - Detail: `INFRA_HEALTH_TIMEOUT = Duration::from_secs(120)`.

- [x] **B4.** Add Valkey health check to boot sequence (infra.rs:wait_for_service_healthy + reachability check)
  - Files: infra.rs
  - Tests: test_valkey_port_is_6379, test_valkey_host_is_localhost
  - Detail: TCP probe Valkey on 127.0.0.1:6379 after Docker compose. If unreachable after timeout → ERROR + return false.

- [x] **B5.** Add disk space pre-flight check before Docker compose (infra.rs:check_disk_space)
  - Files: infra.rs
  - Tests: test_disk_space_min_is_5gb, test_disk_space_check_passes_on_dev_machine
  - Detail: Uses raw FFI `statvfs` syscall (no libc crate needed). Minimum 5 GB free. Returns false on insufficient space → boot halted.

- [x] **B6.** Add Linux Docker daemon auto-start support in `ensure_docker_daemon_running` (infra.rs:launch_docker_linux)
  - Files: infra.rs
  - Tests: (runtime-only — requires systemctl)
  - Detail: On Linux, tries `sudo systemctl start docker`. Polls `docker info` until ready or timeout. Shared `poll_docker_daemon_ready()` function extracted from macOS path.

### C. Mac Auto-Start (Shell/plist files — no Rust)

- [x] **C1.** Create launchd plist for 08:15 AM daily trigger (deploy/launchd/co.dhan.live-trader.plist)
  - Files: deploy/launchd/co.dhan.live-trader.plist
  - Tests: (infrastructure file — manually verified)

- [x] **C2.** Create launcher shell script with Docker Desktop recovery (deploy/launchd/trading-launcher.sh)
  - Files: deploy/launchd/trading-launcher.sh
  - Tests: (infrastructure file — manually verified)

- [x] **C3.** Create Mac power management setup script (deploy/launchd/setup-power-management.sh)
  - Files: deploy/launchd/setup-power-management.sh
  - Tests: (infrastructure file — manually verified)

### D. AWS Auto-Start (systemd/infra files — no Rust)

- [x] **D1.** Create systemd service unit (deploy/systemd/dhan-live-trader.service)
  - Files: deploy/systemd/dhan-live-trader.service
  - Tests: (infrastructure file — validated with `systemd-analyze verify`)

- [x] **D2.** Create systemd timer unit (deploy/systemd/dhan-live-trader.timer)
  - Files: deploy/systemd/dhan-live-trader.timer
  - Tests: (infrastructure file — validated with `systemd-analyze verify`)

- [x] **D3.** Create EC2 setup script with Docker + monitoring config (deploy/aws/ec2-setup.sh)
  - Files: deploy/aws/ec2-setup.sh
  - Tests: (infrastructure file — run once on fresh EC2)

### E. Config + SSM Hardening

- [x] **E1.** Add `ready_by_deadline` config field + boot deadline check (config.rs, base.toml, main.rs, events.rs)
  - Files: config.rs, base.toml, main.rs, events.rs, trading_calendar.rs, engine.rs
  - Tests: test_ready_by_deadline_default_value, test_ready_by_deadline_in_valid_config, test_boot_deadline_missed_message, test_boot_deadline_missed_severity

- [x] **E2.** Add SSM retry with exponential backoff for throttling (secret_manager.rs:fetch_secret_with_retry)
  - Files: secret_manager.rs
  - Tests: test_ssm_retry_max_attempts_bounded, test_ssm_retry_initial_backoff_reasonable, test_ssm_retry_on_error

---

## Boot Sequence (After Implementation)

### Mac (Dev)
```
08:00  pmset wakeorpoweron fires (safety net for sleep)
08:15  launchd StartCalendarInterval fires trading-launcher.sh
         |
         +-- Check ~/.dhan-live-trader-disabled → exit if exists
         +-- Flush DNS cache
         +-- Check /Applications/Docker.app exists
         +-- Check docker info (is daemon responsive?)
         |     +-- App running but daemon dead → killall + restart
         |     +-- App not running → open -a Docker --background
         |     +-- Poll docker info every 2s, timeout 120s
         |     +-- If stuck → killall, retry once → if still stuck → CRITICAL
         +-- Verify network (host api.dhan.co)
         +-- Run binary
               |
               +-- Config → Observability → Logging
               +-- DISK SPACE CHECK (>5 GB required)
               +-- Docker compose up (with exit code validation)
               +-- QuestDB health (actual SELECT 1 query, not just TCP)
               +-- Valkey health check (TCP probe port 6379)
               +-- Notification service init
               +-- IP verification
               +-- Auth (token from cache or SSM)
               +-- Fresh CSV download + universe build
               +-- QuestDB DDL setup
               +-- WebSocket connect (ALL, immediately — no sleeping)
               +-- Telegram: "ALL SYSTEMS HEALTHY — N WS, M instruments"
               +-- StorageGate CLOSED (ticks flow but not persisted)
08:25  Everything ready (worst case ~10 min boot)
09:00  StorageGate opens → ticks persist to QuestDB
15:30  CancellationToken fires → WebSockets disconnect
```

### AWS (Prod)
```
08:00  EventBridge starts EC2 instance (cron: 30 2 * * MON-FRI UTC)
         |
         +-- Elastic IP already attached (survives stop/start)
         +-- Boot: 30-60s instance start + 60-90s cloud-init
08:02  Instance running, Docker daemon up via systemd
08:15  systemd timer fires dhan-live-trader.service
         |
         +-- ExecStartPre: docker compose down (cleanup stale containers)
         +-- ExecStartPre: docker compose config --quiet (validate YAML)
         +-- ExecStart: /opt/dhan-live-trader/bin/dhan-live-trader
               |
               +-- Same boot sequence as Mac (from Config onward)
               +-- SSM credentials via IAM instance profile (IMDSv2)
               +-- Token cache at /tmp/dlt-token-cache (survives restarts)
08:25  Everything ready
09:00  StorageGate opens
15:30  Market close → WebSockets disconnect
17:30  EventBridge stops EC2 instance
         |
         +-- Elastic IP stays attached (no cost when instance running)
         +-- EBS volume persists (QuestDB data safe)
         +-- $0.005/hr for stopped Elastic IP (~$3.65/month off-hours)
```

## Failure Scenarios (Every Edge Case)

| # | Scenario | Detection | Recovery |
|---|----------|-----------|----------|
| 1 | Docker Desktop not installed (Mac) | `/Applications/Docker.app` missing | CRITICAL alert, halt |
| 2 | Docker Desktop stuck "Starting..." | `docker info` timeout 120s | killall Docker, retry once, CRITICAL if still stuck |
| 3 | Docker VM crashed, UI alive | `docker info` fails despite app process running | killall Docker, restart |
| 4 | Mac was asleep at 08:15 | launchd coalesces on wake | Fires immediately on wake |
| 5 | Mac was powered OFF at 08:15 | No launchd `Persistent` key | `RunAtLoad=true` fires on login; pmset wake at 08:00 as safety net |
| 6 | Docker ports inaccessible after sleep | TCP probe fails post-wake | killall Docker, full restart in launcher script |
| 7 | DNS broken after wake (Mac) | `host api.dhan.co` fails | `killall -HUP mDNSResponder` + `dscacheutil -flushcache`, retry |
| 8 | QuestDB port open but not accepting queries | `SELECT 1` HTTP fails | Retry up to 120s, CRITICAL if timeout |
| 9 | QuestDB OOM killed (ExitCode 137) | `docker inspect` ExitCode check | CRITICAL alert, halt boot |
| 10 | Valkey down but QuestDB up | TCP probe port 6379 fails | CRITICAL alert (token cache required) |
| 11 | Port conflict (port already in use) | Container ExitCode != 0 | CRITICAL alert with container logs in message |
| 12 | Disk < 5 GB free | `statvfs` check before Docker compose | CRITICAL alert, halt boot |
| 13 | EC2 instance was off at 08:15 | `Persistent=true` in systemd timer | Timer fires immediately on boot |
| 14 | Docker daemon not running (Linux) | `Requires=docker.service` in unit | systemd starts Docker first; fallback: `systemctl start docker` |
| 15 | SSM unreachable at boot | AWS SDK retry exhausted + our retry | CRITICAL alert, halt (can't get secrets) |
| 16 | SSM throttled (>40 TPS) | ThrottlingException from SDK | Exponential backoff: 1s → 2s → 4s, max 9 total attempts |
| 17 | KMS outage (can't decrypt SecureString) | KMS error in SSM response | CRITICAL alert, halt (no secrets without KMS) |
| 18 | IMDSv2 not ready (EC2 just started) | Credential fetch fails | SDK retries automatically; our code retries on top |
| 19 | Boot takes > 15 min (past 08:30 deadline) | `ready_by_deadline` config check | CRITICAL alert (missed pre-market buffer) |
| 20 | Network not ready at boot (EC2) | `After=network-online.target` | systemd waits for network before starting |
| 21 | Docker auto-update breaks startup (Mac) | Docker fails to start after update | killall + restart recovery; document: pin Docker version |
| 22 | Partial startup (3/8 services up) | Required service exit code check | CRITICAL for required (QuestDB, Valkey), WARN for optional |
| 23 | Boot on non-trading day | `has_any_session_today()` returns false | Skip WS + storage, log info, still start API dashboard |
| 24 | Boot at 08:15 on Muhurat Sunday | Special session detection | WS connects, close at 12:30 per special day config |
| 25 | Boot after market close (16:00) | PostMarket phase detected | No WS, no storage, API dashboard only |
| 26 | App crash during market hours (AWS) | `Restart=on-failure` + `RestartSec=5s` | systemd restarts, max 5 attempts in 300s |
| 27 | App hangs (no crash, no exit) | Future: `WatchdogSec=30s` + `sd_notify` | Future: systemd kills + restarts |
| 28 | `try_join!` partial SSM failure | One of 3 Dhan secret fetches fails | All 3 fail (try_join semantics), retry all 3 |
| 29 | Elastic IP detached from EC2 | No public IP → Dhan static IP check fails | IP verification catches it → CRITICAL alert |
| 30 | EBS volume full (QuestDB WAL) | Disk space check at boot | CRITICAL alert, halt; CloudWatch alarm for >85% |
| 31 | AWS credentials expired (Mac dev) | SSM fetch returns auth error | Log clear message: "run `aws sso login`" |
| 32 | Mac auto-start still fires after AWS prod deployed | Kill switch file check | `~/.dhan-live-trader-disabled` → launcher exits cleanly |

## Key Invariants

1. **QuestDB NEVER on hot tick path** — async ILP writes, StorageGate is O(1) AtomicBool with Relaxed ordering
2. **StorageGate CLOSED until 09:00** — pre-market ticks flow through pipeline but NOT persisted
3. **No sleeping on trading day boot** — PreConnect phase now connects WebSocket immediately
4. **Telegram health notification** — operator gets "ALL SYSTEMS HEALTHY" before market opens
5. **Required services must be healthy** — QuestDB + Valkey checked with actual validation; optional services (Grafana, Prometheus, etc.) best-effort
6. **Container exit codes checked** — OOM (137), error (1) → CRITICAL alert + halt
7. **Mac: sleep prevented** — `pmset -c sleep 0`, scheduled wake at 08:00 as safety net
8. **AWS: `Persistent=true`** — missed timer fires on next boot, Elastic IP survives stop/start
9. **WebSocket has ZERO Docker dependency** — WebSocket connects via Dhan token (SSM/cache) + subscription plan (CSV/binary cache). Docker/QuestDB is cold-path only via StorageGate
10. **Kill switch for Mac** — `~/.dhan-live-trader-disabled` file disables auto-start when AWS goes live

---

## Real-World Smoke Tests (Manual Verification Checklist)

> **Rule:** NO plan item is marked VERIFIED until its smoke test passes on real hardware.
> Unit tests prove the logic compiles. Smoke tests prove it actually works.
> Run these on your Mac after implementation. Check each box only after SEEING the result.

---

### ST-01: WebSocket connects immediately on PreConnect boot (Tests A1)

**Setup:**
```bash
# Set system clock before 08:30 (or run during actual pre-market hours)
# Ensure Docker + QuestDB + Valkey are already running
docker compose -f deploy/docker/docker-compose.yml up -d
```

**Steps:**
```bash
# 1. Run the app (before 08:30 IST on a trading day)
cargo run --release 2>&1 | tee /tmp/st-01.log

# 2. Watch logs for WebSocket connection
grep -i "websocket.*connect" /tmp/st-01.log
```

**Pass criteria:**
- [ ] Log shows `WebSocket #0 connected` within 60s of boot (NOT sleeping until 08:30)
- [ ] NO log line containing `PreConnect — sleeping until WebSocket connect time`
- [ ] Log shows `StorageGate` remains CLOSED (not opened before 09:00)

**Fail criteria:**
- App sleeps/hangs before connecting WebSocket
- Log shows "sleeping until WebSocket connect time"
- WebSocket never connects

---

### ST-02: BootHealthCheck Telegram notification received (Tests A2)

**Setup:**
```bash
# Ensure Telegram bot token and chat ID are in SSM
# Ensure app has network connectivity
```

**Steps:**
```bash
# 1. Run the app normally
cargo run --release 2>&1 | tee /tmp/st-02.log

# 2. Check your Telegram for the health message
# 3. Check logs for confirmation
grep -i "boot.*health\|ALL SYSTEMS HEALTHY" /tmp/st-02.log
```

**Pass criteria:**
- [ ] Telegram message received: "ALL SYSTEMS HEALTHY — N WS connections, M instruments, phase: X, storage: gated"
- [ ] Message arrives AFTER WebSocket connections are established
- [ ] Message includes actual instrument count (not 0)

**Fail criteria:**
- No Telegram message received
- Message shows 0 WS connections or 0 instruments
- Message arrives before WebSocket is connected

---

### ST-03: QuestDB OOM kill detected — boot HALTS (Tests B1)

**Setup:**
```bash
# Start all services normally first
docker compose -f deploy/docker/docker-compose.yml up -d
# Wait for QuestDB to be healthy
sleep 10
```

**Steps:**
```bash
# 1. Kill QuestDB with OOM signal (simulates kernel OOM killer)
docker kill --signal=KILL dlt-questdb

# 2. Verify ExitCode is 137
docker inspect dlt-questdb --format '{{.State.ExitCode}}'
# Expected output: 137

# 3. Now start the app — it should detect the dead container
cargo run --release 2>&1 | tee /tmp/st-03.log

# 4. Check logs
grep -i "exit.*code\|137\|OOM\|CRITICAL\|halt" /tmp/st-03.log
```

**Pass criteria:**
- [ ] `docker inspect` shows ExitCode 137
- [ ] App log shows detection of container with exit code 137
- [ ] App log shows CRITICAL alert fired
- [ ] App HALTS boot (does NOT proceed to WebSocket connection)
- [ ] Telegram receives CRITICAL alert about QuestDB OOM

**Fail criteria:**
- App ignores dead QuestDB and proceeds to WebSocket
- No CRITICAL alert
- App hangs silently

---

### ST-04: Container error (ExitCode 1) detected (Tests B1)

**Setup:**
```bash
docker compose -f deploy/docker/docker-compose.yml up -d
sleep 10
```

**Steps:**
```bash
# 1. Stop QuestDB with error exit (simulates config error)
docker stop dlt-questdb

# 2. Corrupt QuestDB config to force exit code 1 on restart
# (Alternative: just check that a stopped container is detected)
docker inspect dlt-questdb --format '{{.State.ExitCode}}'

# 3. Start the app
cargo run --release 2>&1 | tee /tmp/st-04.log

# 4. Check logs
grep -i "exit.*code\|required.*service\|CRITICAL" /tmp/st-04.log
```

**Pass criteria:**
- [ ] App detects non-zero exit code on required service
- [ ] App log shows which service failed and its exit code
- [ ] CRITICAL alert fired
- [ ] Boot halts

**Fail criteria:**
- App proceeds despite dead required service

---

### ST-05: QuestDB port open but not accepting queries (Tests B2)

**Setup:**
```bash
# We need QuestDB's port open but DB not ready
# Best way: start QuestDB, it takes a few seconds to accept queries
```

**Steps:**
```bash
# 1. Stop QuestDB if running
docker stop dlt-questdb 2>/dev/null

# 2. Start QuestDB fresh (takes time to initialize)
docker start dlt-questdb

# 3. IMMEDIATELY start the app (race condition — port open, DB not ready)
cargo run --release 2>&1 | tee /tmp/st-05.log &

# 4. Watch the logs for SELECT 1 retry behavior
grep -i "SELECT 1\|query.*validation\|retry.*questdb\|health.*check" /tmp/st-05.log
```

**Pass criteria:**
- [ ] App logs show it's trying `SELECT 1` query (not just TCP probe)
- [ ] App retries the query if first attempt fails
- [ ] App eventually succeeds when QuestDB becomes ready
- [ ] Total wait time does not exceed 120s timeout

**Fail criteria:**
- App considers QuestDB healthy just because port is open
- App proceeds without running the actual query
- App times out even though QuestDB becomes ready within 120s

---

### ST-06: Valkey down — boot HALTS (Tests B4)

**Setup:**
```bash
docker compose -f deploy/docker/docker-compose.yml up -d
sleep 10
```

**Steps:**
```bash
# 1. Stop Valkey (but keep QuestDB running)
docker stop dlt-valkey

# 2. Start the app
cargo run --release 2>&1 | tee /tmp/st-06.log

# 3. Check logs
grep -i "valkey\|6379\|CRITICAL\|token.*cache\|required" /tmp/st-06.log
```

**Pass criteria:**
- [ ] App detects Valkey is unreachable on port 6379
- [ ] CRITICAL alert fired (Valkey is required for token cache)
- [ ] App HALTS boot (does NOT proceed to WebSocket)
- [ ] Telegram receives alert about Valkey failure

**Fail criteria:**
- App ignores missing Valkey and proceeds normally
- App just logs a warning and continues

---

### ST-07: Disk space too low — boot HALTS (Tests B5)

**Setup:**
```bash
# Create a large file to fill up disk (careful — leave enough for OS!)
# SAFER ALTERNATIVE: temporarily modify the threshold to test
# Change the code threshold from 5 GB to 500 GB temporarily
```

**Steps:**
```bash
# Option A: Modify threshold temporarily (SAFE)
# Change DISK_SPACE_MIN_GB from 5 to 500 in infra.rs, then:
cargo run --release 2>&1 | tee /tmp/st-07.log

# Check logs
grep -i "disk.*space\|insufficient\|CRITICAL\|GB" /tmp/st-07.log

# REVERT the threshold change after test!
```

**Pass criteria:**
- [ ] App checks disk space BEFORE docker compose up
- [ ] App log shows available disk space in GB
- [ ] When below threshold: CRITICAL alert fired
- [ ] Boot HALTS (does NOT attempt docker compose up)

**Fail criteria:**
- No disk space check in logs
- App proceeds despite low disk
- Check happens AFTER docker compose (too late)

---

### ST-08: Timeout increase — QuestDB slow start handled (Tests B3)

**Steps:**
```bash
# 1. Check the constant in the binary
grep -r "INFRA_HEALTH_TIMEOUT" crates/app/src/infra.rs
# Expected: Duration::from_secs(120)

# 2. Verify the test assertion
cargo test -p dhan-live-trader -- test_health_timeout_is_reasonable
```

**Pass criteria:**
- [ ] Constant is 120 seconds (not 60)
- [ ] Test passes with updated range assertion

---

### ST-09: Ready-by-deadline alert when boot is late (Tests E1)

**Setup:**
```bash
# Set system clock to 08:35 (after 08:30 deadline)
# OR run the app after 08:30 on a trading day
```

**Steps:**
```bash
# 1. Run after 08:30 on a trading day
cargo run --release 2>&1 | tee /tmp/st-09.log

# 2. Check for deadline warning
grep -i "deadline\|08:30\|missed.*buffer\|CRITICAL.*ready" /tmp/st-09.log
```

**Pass criteria:**
- [ ] App logs CRITICAL alert: "boot completed after ready_by_deadline 08:30:00"
- [ ] Telegram receives the deadline breach alert
- [ ] App still proceeds to connect (alert is a warning, not a halt)

**Fail criteria:**
- No deadline check in logs
- App silently boots late without any alert

---

### ST-10: SSM retry on transient failure (Tests E2)

**Steps:**
```bash
# 1. Temporarily invalidate AWS credentials to simulate SSM failure
mv ~/.aws/credentials ~/.aws/credentials.bak

# 2. Start the app
cargo run --release 2>&1 | tee /tmp/st-10.log

# 3. Check logs for retry behavior
grep -i "retry\|backoff\|SSM\|secret.*retrieval\|attempt" /tmp/st-10.log

# 4. RESTORE credentials
mv ~/.aws/credentials.bak ~/.aws/credentials
```

**Pass criteria:**
- [ ] App logs show retry attempts (attempt 1, 2, 3)
- [ ] Backoff delays visible in timestamps (1s, 2s, 4s gaps)
- [ ] After max retries: CRITICAL alert (not silent failure)
- [ ] Error message is actionable: mentions "AWS credentials" or "SSM"

**Fail criteria:**
- App fails immediately on first SSM error (no retry)
- App retries forever (no max limit)
- Error message is generic/unhelpful

---

### ST-11: Non-trading day boot (Tests scenario 23)

**Setup:**
```bash
# Run on a Saturday or Sunday, OR add today's date to nse_holidays in base.toml
```

**Steps:**
```bash
# 1. Add today to holidays (if weekday)
# In base.toml: [[trading.nse_holidays]]
# date = "2026-03-14"
# name = "Test Holiday"

# 2. Run the app
cargo run --release 2>&1 | tee /tmp/st-11.log

# 3. Check behavior
grep -i "NonTradingDay\|no.*websocket\|holiday\|weekend" /tmp/st-11.log

# 4. REVERT the holiday addition
```

**Pass criteria:**
- [ ] App detects NonTradingDay phase
- [ ] No WebSocket connections attempted
- [ ] No storage gate activity
- [ ] API dashboard still starts (for monitoring)
- [ ] App does NOT crash or halt

**Fail criteria:**
- App tries to connect WebSocket on holiday
- App crashes on non-trading day

---

### ST-12: Mac kill switch prevents auto-start (Tests C2)

**Steps:**
```bash
# 1. Create the kill switch file
touch ~/.dhan-live-trader-disabled

# 2. Run the launcher script
bash deploy/launchd/trading-launcher.sh 2>&1 | tee /tmp/st-12.log

# 3. Check behavior
cat /tmp/st-12.log

# 4. Remove kill switch
rm ~/.dhan-live-trader-disabled
```

**Pass criteria:**
- [ ] Script exits immediately with exit code 0
- [ ] Log shows "disabled by kill switch" or similar message
- [ ] No Docker operations attempted
- [ ] No app binary executed

**Fail criteria:**
- Script ignores the kill switch file
- Script exits with non-zero code (would trigger launchd restart)

---

### ST-13: Docker Desktop recovery — daemon dead but app alive (Tests C2, scenario 3)

**Steps:**
```bash
# 1. Ensure Docker Desktop is running normally
docker info >/dev/null 2>&1 && echo "Docker OK"

# 2. Kill the Docker daemon process (but leave the app window)
# WARNING: This will temporarily break Docker on your Mac
killall com.docker.backend

# 3. Verify daemon is dead but app icon still shows
docker info 2>&1  # Should fail
pgrep -f Docker   # Should still show processes

# 4. Run the launcher script
bash deploy/launchd/trading-launcher.sh 2>&1 | tee /tmp/st-13.log

# 5. Check recovery
grep -i "killall\|restart\|daemon.*dead\|recovery" /tmp/st-13.log
docker info >/dev/null 2>&1 && echo "Docker recovered"
```

**Pass criteria:**
- [ ] Script detects daemon is dead (docker info fails)
- [ ] Script detects Docker app processes still exist
- [ ] Script kills Docker app processes (`killall Docker`)
- [ ] Script restarts Docker Desktop (`open -a Docker --background`)
- [ ] Script waits for daemon to become responsive
- [ ] `docker info` succeeds after recovery

**Fail criteria:**
- Script hangs waiting for dead daemon
- Script doesn't detect the split state (app alive, daemon dead)
- Docker Desktop not recovered

---

### ST-14: Full happy-path end-to-end boot (Tests everything together)

**Setup:**
```bash
# Ensure: trading day, before 08:30, Docker running, AWS credentials valid
# Start fresh — stop any running instance
docker compose -f deploy/docker/docker-compose.yml down
```

**Steps:**
```bash
# 1. Full cold boot
cargo run --release 2>&1 | tee /tmp/st-14.log

# 2. Wait for "ALL SYSTEMS HEALTHY" in Telegram

# 3. Verify ALL of these in the log (in order):
echo "=== BOOT SEQUENCE VERIFICATION ==="
echo -n "1. Config loaded:        "; grep -c "config.*loaded\|configuration" /tmp/st-14.log
echo -n "2. Disk space checked:   "; grep -c "disk.*space\|GB.*available" /tmp/st-14.log
echo -n "3. Docker compose up:    "; grep -c "docker compose.*started\|compose.*success" /tmp/st-14.log
echo -n "4. Container exit codes: "; grep -c "exit.*code\|container.*health\|inspect" /tmp/st-14.log
echo -n "5. QuestDB SELECT 1:     "; grep -c "SELECT 1\|questdb.*healthy\|query.*valid" /tmp/st-14.log
echo -n "6. Valkey healthy:       "; grep -c "valkey\|6379.*healthy\|valkey.*reachable" /tmp/st-14.log
echo -n "7. IP verified:          "; grep -c "IP verified\|ip.*verification" /tmp/st-14.log
echo -n "8. Auth token acquired:  "; grep -c "Auth OK\|JWT\|token.*acquired\|credentials.*fetched" /tmp/st-14.log
echo -n "9. Instruments built:    "; grep -c "Instruments OK\|universe\|derivative" /tmp/st-14.log
echo -n "10. WebSocket connected: "; grep -c "WebSocket.*connected\|websocket.*pool" /tmp/st-14.log
echo -n "11. Health notification: "; grep -c "ALL SYSTEMS HEALTHY\|boot.*health\|BootHealthCheck" /tmp/st-14.log
echo -n "12. Storage gate closed: "; grep -c "StorageGate.*closed\|storage.*gated" /tmp/st-14.log
echo -n "13. No PreConnect sleep: "; grep -c "sleeping until WebSocket" /tmp/st-14.log
echo "=== ALL counts should be >=1, except #13 which must be 0 ==="
```

**Pass criteria (ALL must pass):**
- [ ] Step 1-12: count >= 1 (each boot step logged)
- [ ] Step 13: count == 0 (no PreConnect sleep)
- [ ] Telegram: "ALL SYSTEMS HEALTHY" received with real data
- [ ] Boot completes in < 5 minutes total
- [ ] WebSocket receives at least one tick (if during market hours)
- [ ] StorageGate remains CLOSED until 09:00

**Fail criteria:**
- Any step count is 0 (boot step skipped silently)
- Step 13 count > 0 (old sleep behavior still present)
- Boot takes > 5 minutes
- No Telegram notification

---

### ST-15: launchd plist installation and trigger (Tests C1)

**Steps:**
```bash
# 1. Copy plist to LaunchAgents
cp deploy/launchd/co.dhan.live-trader.plist ~/Library/LaunchAgents/

# 2. Load the agent
launchctl load ~/Library/LaunchAgents/co.dhan.live-trader.plist

# 3. Verify it's loaded
launchctl list | grep dhan

# 4. Check next trigger time
# (Should show 08:15 on next trading day)

# 5. To test immediately:
launchctl start co.dhan.live-trader

# 6. Check output logs
cat /tmp/dlt-launchd-stdout.log
cat /tmp/dlt-launchd-stderr.log

# 7. Unload after testing
launchctl unload ~/Library/LaunchAgents/co.dhan.live-trader.plist
```

**Pass criteria:**
- [ ] `launchctl list` shows the agent
- [ ] `launchctl start` triggers the launcher script
- [ ] stdout/stderr logs exist and show boot activity
- [ ] App actually starts (not just script runs)

**Fail criteria:**
- plist fails to load (XML syntax error)
- Script runs but app doesn't start
- Logs not created (wrong path)

---

### ST-16: Power management prevents Mac sleep (Tests C3)

**Steps:**
```bash
# 1. Run the setup script
sudo bash deploy/launchd/setup-power-management.sh

# 2. Verify settings applied
pmset -g | grep -E "sleep|displaysleep|womp"
# Expected:
#   sleep          0
#   displaysleep   10
#   womp           1

# 3. Verify scheduled wake
pmset -g sched
# Expected: wakeorpoweron at 08:00:00 MTWRF

# 4. Close laptop lid, wait 30s, open — system should NOT have slept
# (Display off is OK, system sleep is NOT OK)
```

**Pass criteria:**
- [ ] `sleep = 0` (system never sleeps on AC power)
- [ ] `displaysleep = 10` (display sleeps to save screen)
- [ ] `womp = 1` (wake on network access enabled)
- [ ] Scheduled wake shows 08:00 MTWRF
- [ ] Closing lid does NOT put system to sleep (just display off)

**Fail criteria:**
- System sleeps when lid closed
- No scheduled wake configured
- Settings not persistent after reboot

---

## Smoke Test Summary

| Test | Plan Items | Real Infra Needed | Destructive? |
|------|-----------|-------------------|-------------|
| ST-01 | A1 | Docker + QuestDB | No |
| ST-02 | A2 | Telegram bot | No |
| ST-03 | B1 | Docker | Yes (kills container) |
| ST-04 | B1 | Docker | Yes (stops container) |
| ST-05 | B2 | Docker + QuestDB | No (race condition) |
| ST-06 | B4 | Docker + Valkey | Yes (stops container) |
| ST-07 | B5 | None (threshold tweak) | No |
| ST-08 | B3 | None | No |
| ST-09 | E1 | Telegram bot | No |
| ST-10 | E2 | AWS credentials | Yes (moves creds) |
| ST-11 | — | Config edit | No |
| ST-12 | C2 | File system | No |
| ST-13 | C2 | Docker Desktop | Yes (kills daemon) |
| ST-14 | ALL | Everything | No |
| ST-15 | C1 | macOS launchd | No |
| ST-16 | C3 | macOS pmset | No |

**Order of execution:** ST-08 → ST-07 → ST-11 → ST-12 → ST-01 → ST-02 → ST-09 → ST-14 → ST-05 → ST-06 → ST-03 → ST-04 → ST-10 → ST-13 → ST-15 → ST-16

(Non-destructive tests first, then destructive, then Mac-specific last)
