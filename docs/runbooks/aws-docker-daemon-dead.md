# Runbook — QuestDB / Docker down → app crash-loops (BOOT-02)

> **Symptom:** the tickvault app sits in `activating` with **0 ticks**, the journal
> repeats `Failed to start tickvault.service` + `BOOT-02: QuestDB not ready`, and the
> AWS-autopilot Telegram says **"QuestDB container not running after docker compose up"**.
> **One-line cause:** the app is fail-closed — it refuses to run without QuestDB, and
> **QuestDB is not up**. Get QuestDB up and the app self-recovers on its next restart.
> **Referenced by:** `docs/architecture/aws-daily-lifecycle.md`.

---

## Why the app can't "just start"

| Fact | Consequence |
|---|---|
| App is `Type=notify`, fail-closed at boot step 6c | No QuestDB → exits with **BOOT-02** → `Restart=always` loops forever |
| `user-data` (`docker compose up -d`) runs **once** (cloud-init) | A stop/start does NOT recreate a removed QuestDB container |
| App unit waits for the Docker **daemon**, not the QuestDB **container** | App boots before/without QuestDB and loops |

The systemd unit now has `ExecStartPre=… docker compose up -d questdb` (self-heal for the
**removed-container** case) and the portal **Restart QuestDB** button now uses `up -d` (create-or-restart).

---

## The decision tree (run on the box: SSM, or the portal AWS/Logs tab)

```
df -h                              # 1) DISK
docker ps -a                       # 2) container state
docker logs tv-questdb --tail 80   # 3) why QuestDB exits
```

| What you see | Cause | Fix |
|---|---|---|
| `/` at ~100% **or** `docker logs` shows **"No space left on device"** | **Disk full (EBS)** — the #1 cause | Free space + (optionally) grow EBS — see below |
| `docker ps -a` shows `tv-questdb` **missing** | Container removed (down/wipe/reset) | `cd /opt/tickvault/repo/deploy/docker && docker compose up -d questdb` |
| `tv-questdb` **Exited / Restarting**, logs show a **corruption / WAL / table open** error | **Volume corruption** (unclean stop mid-write) | repair / restore — see below |
| `systemctl status docker` not `active` | Docker daemon down (often disk-full too) | `sudo systemctl restart docker` then `compose up -d` |

> **"QuestDB not running after `docker compose up`"** (the autopilot's exact words) means the
> container was created but **exited immediately** → it's **crashing on start**, i.e. the
> **disk-full** or **volume-corruption** row above. `compose up` cannot fix either.

---

## Fix: disk full (the usual case)

QuestDB writes to `tv-questdb-data` (`/var/lib/questdb`) on the 30 GB gp3 root volume. Within the
90-day retention window the disk only grows (there's a CloudWatch **>75% full** alarm for this).

1. **See what's big:** `sudo du -xh --max-depth=2 / | sort -rh | head -20` — usual culprits:
   `/var/lib/docker/volumes/tv-questdb-data` (QuestDB data), `/opt/tickvault/data/spill`, `/opt/tickvault/data/logs`.
2. **Free space (safe):** clear spill/DLQ + old logs first —
   `rm -f /opt/tickvault/data/spill/* /opt/tickvault/data/dlq/* ; find /opt/tickvault/data/logs -name 'app.*' -mtime +2 -delete`.
3. **If still full:** drop old QuestDB partitions (data >90d should already be in S3), or **grow the volume online** (no downtime):
   - bump `ebs_gp3_size_gb` (e.g. `30 → 50`) in `deploy/aws/terraform/variables.tf` + `terraform apply`,
   - on the box: `sudo growpart /dev/nvme0n1 1 && sudo xfs_growfs /`.
4. **Bring it up:** `cd /opt/tickvault/repo/deploy/docker && docker compose up -d questdb` → the app's
   `Restart=always` loop succeeds on its next attempt (≤60s). Watch `journalctl -u tickvault -f`.

## Fix: volume corruption

1. `docker logs tv-questdb --tail 120` → identify the offending table / WAL.
2. If a single table is corrupt, QuestDB may start with it quarantined; otherwise restore the
   `tv-questdb-data` volume from the most recent backup/snapshot.
3. Worst case (data is reproducible — ticks are re-fetchable): the portal **"Wipe ALL data → fresh
   start"** recreates an empty volume + restarts. Audit tables are preserved by that action.

---

## After recovery — confirm

- `systemctl is-active tickvault` → `active`
- portal app tile → `running`, and **minute candles are sealing** (`candles_1m` row-count climbing)
- `journalctl -u tickvault -f` → no more `BOOT-02`

---

## Where to watch logs (AWS, end-to-end)

| Layer | Where | What |
|---|---|---|
| App log + `errors.jsonl` (incl. the `BOOT-02` `error!`) | **CloudWatch → Logs → Live tail → `/tickvault/<env>/app`** (ap-south-1) | the IntelliJ-style live app stream |
| systemd / `Failed to start` / raw boot lines | **journald** — portal **Logs tab**, or `journalctl -u tickvault` via SSM | unit + crash-loop |
| QuestDB's own crash reason | `docker logs tv-questdb` (box) | why the container exits — **not yet shipped to CloudWatch** (future enhancement) |

---

## Prevention (shipped / planned)

- **Shipped:** `ExecStartPre … up -d questdb` self-heal; portal Restart-QuestDB uses `up -d`; the
  CloudWatch agent now tails `/opt/tickvault/data/logs/` (the real path) so app+error logs reach
  CloudWatch; the **>75% disk** CloudWatch alarm warns before full.
- **Planned:** ship QuestDB container logs to CloudWatch (via the `awslogs` driver, with a
  pre-created log group so a bad driver config can never block the container).

---

## Cross-references

- `docs/runbooks/aws-capacity-error.md` — EC2 won't start (capacity).
- `docs/runbooks/aws-startinstances-failed.md` — start-instances failures.
- `docs/architecture/aws-daily-lifecycle.md` — the daily start/stop lifecycle.
