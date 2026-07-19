#!/usr/bin/env bash
# =============================================================================
# ensure-questdb.sh — make the tv-questdb container RUNNING, robustly.
# =============================================================================
# Idempotent, cold-path. Safe to call from:
#   - deploy/systemd/tickvault.service  ExecStartPre (boot self-heal)
#   - crates/aws-lambdas/src/operator_control.rs  (restart-questdb / docker-reset; the python handler.py was retired 2026-07-18, rust-only phase 2b-3)
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
# Single real env (operator 2026-06-30): default prod.
ENV="${TV_ENVIRONMENT:-prod}"
REGION="${AWS_REGION:-${AWS_DEFAULT_REGION:-ap-south-1}}"
COMPOSE_FILE="${TV_COMPOSE_FILE:-/opt/tickvault/repo/deploy/docker/docker-compose.yml}"
IMAGE="questdb/questdb:9.3.5"

# SERVICE-USER PATH hardening (2026-07-13, deploy-hang fix companion): under
# the systemd unit (ExecStartPre, User=ec2-user) PATH can be the minimal
# systemd default and miss /usr/local/bin — where `docker-compose` (v1) and
# the aws CLI commonly live — so the fallback ladder below silently skipped
# rungs in exactly the boot context that needs them most (post-nuke 08:30
# boot with no container). Prepending the standard dirs is a no-op in an
# SSM/root or operator shell that already has them.
PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${PATH}"
export PATH

log() { echo "ensure-questdb: $*"; }

# Per-call timeout for every blocking `docker` invocation (2026-06-30): a wedged
# Docker daemon makes `docker ...` hang INDEFINITELY. With no timeout this script
# — run as systemd ExecStartPre (deploy/systemd/tickvault.service) — would hang
# forever; PR #1275 set TimeoutStartSec=infinity (so the by-design infinite
# CSV-build retry isn't SIGTERM-killed), which ALSO means systemd will not kill a
# hung ExecStartPre. So we bound each docker call ourselves: on timeout, `timeout`
# returns exit 124, the call is treated as failed, and the script exits non-zero
# → systemd's Restart=always recovers a wedged-daemon hang instead of the box
# sitting silently. DOCKER_CALL_TIMEOUT_SECS is overridable for slow cold starts.
DOCKER_CALL_TIMEOUT_SECS="${DOCKER_CALL_TIMEOUT_SECS:-90}"
# dtimeout wraps `docker` with a hard wall-clock bound. `timeout` sends TERM then
# KILL after the deadline; exit 124 = timed out. If `timeout` itself is missing
# (minimal image), fall back to a bare `docker` so the happy path never breaks.
dtimeout() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "${DOCKER_CALL_TIMEOUT_SECS}s" docker "$@"
  else
    docker "$@"
  fi
}

# 1. Already running? Nothing to do.
if [ "$(dtimeout inspect -f '{{.State.Running}}' "$SVC" 2>/dev/null)" = "true" ]; then
  log "$SVC already running"
  exit 0
fi

# 2. Exists but stopped → start it (reuses the container's original env/config,
#    so no SSM creds are needed for this common case).
if dtimeout inspect "$SVC" >/dev/null 2>&1; then
  if dtimeout start "$SVC" >/dev/null 2>&1; then
    log "started existing stopped $SVC"
    exit 0
  fi
  log "docker start failed (or timed out); falling through to recreate"
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

# `timeout` wrapper for the compose binaries (same wedged-daemon bound as
# dtimeout, but the first arg is the compose command, not `docker`).
ctimeout() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "${DOCKER_CALL_TIMEOUT_SECS}s" "$@"
  else
    "$@"
  fi
}

# 3-pre. Bounded image pre-pull with retry (2026-07-13, deploy-hang fix
# companion). Every recreate rung below runs under the 90s
# DOCKER_CALL_TIMEOUT_SECS wedged-daemon bound — but on a post-nuke box with
# NO local image, `up -d` / `run -d` must first PULL ~hundreds of MB, which
# routinely exceeds 90s, so EVERY rung timed out one after another and the
# self-heal failed exactly when it mattered (tomorrow's 08:30 boot →
# BOOT-02 halt). Pull the image EXPLICITLY first: skip instantly when the
# image is already present (the normal case — a deploy must never wait on a
# pull that is not pending), else up to 3 attempts, each hard-bounded, with
# a short breather between attempts. A final pull failure logs LOUDLY and
# still falls through to the recreate rungs (a partially-cached image may
# still succeed; a real failure surfaces at the rung and exits non-zero).
DOCKER_PULL_TIMEOUT_SECS="${DOCKER_PULL_TIMEOUT_SECS:-300}"
if dtimeout image inspect "$IMAGE" >/dev/null 2>&1; then
  log "image $IMAGE already present - skipping pull (nothing pending)"
else
  PULLED=no
  for attempt in 1 2 3; do
    log "pulling $IMAGE (attempt ${attempt}/3, bounded ${DOCKER_PULL_TIMEOUT_SECS}s)"
    if DOCKER_CALL_TIMEOUT_SECS="$DOCKER_PULL_TIMEOUT_SECS" dtimeout pull "$IMAGE" >/dev/null 2>&1; then
      log "image $IMAGE pulled"
      PULLED=yes
      break
    fi
    sleep 10
  done
  if [ "$PULLED" != "yes" ]; then
    log "WARN: could not pull $IMAGE after 3 bounded attempts - continuing to the recreate rungs (they fail loudly if the image is truly absent)"
  fi
fi

# 3a. compose v2 plugin (`docker compose`). Since 2026-07-14 the deploy
#     script (.github/workflows/deploy-aws.yml SSM block) ENSURES this plugin
#     on every deploy — pinned + sha256-verified static install to
#     /usr/local/lib/docker/cli-plugins/docker-compose when the distro repo
#     lacks it. That dir is the docker CLI's system-wide plugin dir, so
#     `docker compose` works for ANY user here (service-user boot included);
#     rung 3c below also probes that exact path directly as belt-and-braces.
if dtimeout compose version >/dev/null 2>&1; then
  if ctimeout docker compose -f "$COMPOSE_FILE" up -d "$SVC"; then
    log "recreated via 'docker compose' (v2)"
    exit 0
  fi
fi
# 3b. compose v1 standalone (`docker-compose`).
if command -v docker-compose >/dev/null 2>&1; then
  if ctimeout docker-compose -f "$COMPOSE_FILE" up -d "$SVC"; then
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
    if ctimeout "$p" -f "$COMPOSE_FILE" up -d "$SVC"; then
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
# `docker run -d` may pull the image on a cold box, so allow more headroom than
# the per-call default (still bounded so a wedged daemon cannot hang forever).
DOCKER_RUN_TIMEOUT_SECS="${DOCKER_RUN_TIMEOUT_SECS:-300}"
# Honor TV_QDB_HTTP_BIND from the compose project's .env (B4 QuestDB console,
# 2026-07-03): the compose paths above pick it up automatically (compose v2
# loads .env from the compose file's dir), but this raw `docker run` fallback
# must read it explicitly — otherwise a self-heal would silently regress
# :9000 back to localhost-only and break the console's VPC back Lambda.
# Dev default stays 127.0.0.1; prod .env sets 0.0.0.0 (box SG admits 9000
# from the console Lambda's SG only — never public).
QDB_HTTP_BIND="${TV_QDB_HTTP_BIND:-}"
if [ -z "$QDB_HTTP_BIND" ] && [ -f "$(dirname "$COMPOSE_FILE")/.env" ]; then
  QDB_HTTP_BIND="$(sed -n 's/^TV_QDB_HTTP_BIND=//p' "$(dirname "$COMPOSE_FILE")/.env" | tail -1)"
fi
QDB_HTTP_BIND="${QDB_HTTP_BIND:-127.0.0.1}"
if DOCKER_CALL_TIMEOUT_SECS="$DOCKER_RUN_TIMEOUT_SECS" dtimeout run -d --name "$SVC" --hostname "$SVC" --restart unless-stopped \
  -p "$QDB_HTTP_BIND:9000:9000" -p 8812:8812 -p 9009:9009 -p 127.0.0.1:9003:9003 \
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
