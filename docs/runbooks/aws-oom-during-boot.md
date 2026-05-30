# Runbook — Tickvault OOM-killed Mid-Boot (Case J)

> **Severity:** Critical.
> **Detection latency:** ≤T+240s via `tv_boot_complete` missing alarm.
> **Companion:** `docs/architecture/aws-daily-lifecycle.md` §3 Case J.

## Symptom

- Instance running; SSH works.
- `journalctl -u tickvault` ends abruptly mid-boot.
- `dmesg | grep -i kill` shows `Out of memory: Killed process <pid> (tickvault)`.

## Why this happens

Boot-time peak memory exceeded the t4g.medium 8GB envelope. Likely causes:

1. rkyv binary cache deserialization spike during universe load.
2. Bug introduced that allocates a large vector at boot.
3. Container memory limits over-allocated (sum > 6GB per `aws-budget.md` rule 6).
   Post CloudWatch-only migration (#O1–#O4) the runtime is QuestDB + the
   tickvault app only, so the container sum is small — but verify nothing
   stale was re-added to docker-compose.

## Immediate actions

```bash
# 1. Confirm
sudo dmesg -T | grep -i 'killed process' | tail -5

# 2. Inspect memory pressure history
sudo journalctl --since "10 min ago" | grep -iE 'mem|oom'

# 3. Check container limits sum
docker ps --format '{{.Names}}' | while read c; do
  docker stats --no-stream --format "{{.Name}}: {{.MemUsage}} / {{.MemLimit}}" $c
done

# 4. If a Docker container is the killer (not tickvault itself) — adjust its limit
# Edit deploy/docker/docker-compose.yml (per aws-budget.md rule 6) and redeploy
```

## If the host itself is the issue (the most likely case)

Per `aws-budget.md` rule 11, host headroom floor is 2GB. If actual headroom < 2GB:

| Action | Effect |
|---|---|
| Trim the QuestDB write buffer / cache during boot | frees a few hundred MB (the Grafana/Prometheus sidecars that used to free RAM here were removed in the CloudWatch-only migration #O1–#O3) |
| Resize to t4g.medium (16GB RAM) | doubles EC2 cost — operator decision |
| Trim tickvault working set | requires code change |

## Acceptance criteria

- App boots without OOM and emits `BootReadyConfirmation`.
- `tv_subsystem_memory_rss_bytes` gauge per subsystem within budget per `aws-budget.md`.

## Post-incident

- If OOM recurs same boot day → assume tickvault has a memory regression.
- Run `crates/app/src/subsystem_memory.rs` audit (Wave-5 §AA L122).
- File adversarial 3-agent review per operator-charter §E.
