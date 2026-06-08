#!/usr/bin/env bash
# =============================================================================
# ensure-questdb.sh — make the tv-questdb container RUNNING, robustly.
# =============================================================================
# Idempotent, cold-path. Safe to call from:
#   - deploy/systemd/tickvault.service  ExecStartPre (boot self-heal)
#   - deploy/aws/lambda/operator-control/handler.py  (restart-questdb / docker-reset)
#   - an operator shell
#
# Why this exists (incident 2026-06-08): PR #1052's self-heal used
#   `docker compose -f ... up -d questdb`
# which had THREE bugs that crash-looped the app on BOOT-02:
#   (a) some hosts have no `docker compose` v2 plugin → docker prints its
#       top-level help and the self-heal is a no-op;
#   (b) the compose SERVICE name is `tv-questdb`, NOT `questdb` — so even with
#       a working plugin, `up -d questdb` errors "no such service";
#   (c) a compose recreate needs QDB_PG_USER/QDB_PG_PASSWORD from SSM, which the
#       systemd unit's environment does not carry.
# This helper fixes all three and degrades gracefully: running → no-op;
# stopped → `docker start` (reuses original env, no creds needed); removed →
# fetch SSM creds + recreate via compose (v2 → v1 → plugin-by-path), and as a
# LAST resort `docker run` a faithful tv-questdb so the box recovers even with
# no compose at all.
#
# Exit 0 = QuestDB is up (or was already). Exit 1 = could not bring it up.
# =============================================================================
set -u

SVC="tv-questdb"
ENV="${TV_ENVIRONMENT:-staging}"
REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-ap-south-1}}"
COMPOSE_FILE="${TV_COMPOSE_FILE:-/opt/tickvault/repo/deploy/docker/docker-compose.yml}"
IMAGE="questdb/questdb:9.3.5"

log() { echo "ensure-questdb: $*"; }

# 1. Already running? Nothing to do.
if [ "$(docker inspect -f '{{.State.Running}}' "$SVC" 2>/dev/null)" = "true" ]; then
  log "$SVC already running"
  exit 0
fi

# 2. Exists but stopped → start it (reuses the container's original env/config,
#    so no SSM creds are needed for this common case).
if docker inspect "$SVC" >/dev/null 2>&1; then
  if docker start "$SVC" >/dev/null 2>&1; then
    log "started existing stopped $SVC"
    exit 0
  fi
  log "docker start failed; falling through to recreate"
fi

# 3. Gone (nuke/reset) → recreate. Fetch the PG creds from SSM first; both the
#    compose path and the docker-run fallback need them.
fetch_ssm_secret() {
  aws ssm get-parameter --with-decryption --region "$REGION" \
    --name "$1" --query 'Parameter.Value' --output text 2>/dev/null
}
TV_QUESTDB_PG_USER="$(fetch_ssm_secret "/tickvault/${ENV}/questdb/pg-user")"
TV_QUESTDB_PG_PASSWORD="$(fetch_ssm_secret "/tickvault/${ENV}/questdb/pg-password")"
export TV_QUESTDB_PG_USER TV_QUESTDB_PG_PASSWORD
if [ -z "${TV_QUESTDB_PG_USER}" ] || [ -z "${TV_QUESTDB_PG_PASSWORD}" ]; then
  log "WARN: could not read QuestDB PG creds from SSM (/tickvault/${ENV}/questdb/*); recreate may fail"
fi

# 3a. compose v2 plugin (`docker compose`).
if docker compose version >/dev/null 2>&1; then
  if docker compose -f "$COMPOSE_FILE" up -d "$SVC"; then
    log "recreated via 'docker compose' (v2)"
    exit 0
  fi
fi
# 3b. compose v1 standalone (`docker-compose`).
if command -v docker-compose >/dev/null 2>&1; then
  if docker-compose -f "$COMPOSE_FILE" up -d "$SVC"; then
    log "recreated via 'docker-compose' (v1)"
    exit 0
  fi
fi
# 3c. compose plugin by absolute path (PATH/plugin-dir not visible to this user).
for p in \
  /usr/local/lib/docker/cli-plugins/docker-compose \
  /usr/libexec/docker/cli-plugins/docker-compose \
  /usr/lib/docker/cli-plugins/docker-compose \
  "${HOME:-/home/ec2-user}/.docker/cli-plugins/docker-compose"; do
  if [ -x "$p" ]; then
    if "$p" -f "$COMPOSE_FILE" up -d "$SVC"; then
      log "recreated via compose plugin at $p"
      exit 0
    fi
  fi
done

# 3d. LAST RESORT: no compose available at all → docker run a faithful
#     tv-questdb. The host app reaches QuestDB via published localhost ports
#     (127.0.0.1:9009 ILP / :8812 PG / :9000 HTTP), so no compose network is
#     needed. Mirrors the compose spec's ports/volume/limits + the PG creds.
if [ -z "${TV_QUESTDB_PG_USER}" ] || [ -z "${TV_QUESTDB_PG_PASSWORD}" ]; then
  log "FAILED: no compose available AND no PG creds — refusing to docker-run without auth"
  exit 1
fi
log "no compose found — recreating $SVC via 'docker run'"
# QuestDB reads QDB_PG_USER / QDB_PG_PASSWORD from its env. Export the
# SSM-fetched values and pass them through with the value-less `-e NAME` form —
# keeps the secret off the command line AND out of the secret-scanner regex.
QDB_PG_USER="$TV_QUESTDB_PG_USER"; export QDB_PG_USER              # secret-scan-ignore: value from AWS SSM
QDB_PG_PASSWORD="$TV_QUESTDB_PG_PASSWORD"; export QDB_PG_PASSWORD  # secret-scan-ignore: value from AWS SSM
if docker run -d --name "$SVC" --hostname "$SVC" --restart unless-stopped \
  -p 127.0.0.1:9000:9000 -p 8812:8812 -p 9009:9009 -p 127.0.0.1:9003:9003 \
  -v tv-questdb-data:/var/lib/questdb \
  --shm-size 512m --memory 2g \
  -e QDB_TELEMETRY_ENABLED=false \
  -e QDB_METRICS_ENABLED=TRUE \
  -e QDB_PG_USER \
  -e QDB_PG_PASSWORD \
  "$IMAGE" >/dev/null; then
  log "recreated via 'docker run'"
  exit 0
fi

log "FAILED to bring up $SVC by any method"
exit 1
