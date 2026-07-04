#!/usr/bin/env bash
# =============================================================================
# local-autopilot.sh — ZERO-TOUCH local trading-day orchestrator
# (local-runtime branch ONLY; operator 2026-07-04: "nope i won't run any
#  command since everything is on local testing make everything automatic")
# =============================================================================
# Subcommands:
#   run    — the unattended trading-day orchestrator (launchd fires this at
#            08:55 IST on weekdays; safe to fire any time — it classifies the
#            day and no-ops quietly on weekends/holidays).
#   start  — MANUAL start (operator addition 2026-07-04: "let me autostart or
#            autostop manually also"): docker + app + caffeinate. Clears the
#            manual-stop marker and is IDEMPOTENT — running services are
#            detected, never double-started.
#   stop   — MANUAL stop: graceful app stop + scale-test-clean + writes
#            data/local-manual-stop.marker so the autopilot NEVER auto-
#            relaunches after a manual stop (until local-start clears it or
#            the next trading day begins). Docker is left up.
#   status — one-glance state (app pid, docker, marker, mode).
#
# MANUAL ALWAYS WINS: the autopilot monitor loop checks the marker every
# iteration and stands down the moment it appears.
#
# Telegram: best-effort via scripts/notify-telegram.sh (SSM-backed). A failed
# send never blocks the run — the message is always in the autopilot log too.
#
# Testing: `AUTOPILOT_LIB=1 source scripts/local-autopilot.sh` loads the pure
# decision functions WITHOUT executing anything (scripts/local-autopilot-test.sh).
# =============================================================================

# ── Tunables (env-overridable) ──────────────────────────────────────────────
AUTOPILOT_DIR="${AUTOPILOT_DIR:-data/local-autopilot}" # runtime STATE (pids, markers, lock) — NOT logs
# FIX 4 (operator 2026-07-04): ONE human log. All launcher output appends
# into the SAME daily rolling file the app uses (data/logs/app.<date>.log,
# recomputed per write — see current_log). ONE_FILE_LINK is the fixed name
# the operator always opens — a symlink kept pointed at today's file.
ONE_FILE_LINK="${ONE_FILE_LINK:-data/logs/tickvault.log}"
MANUAL_STOP_MARKER="${MANUAL_STOP_MARKER:-data/local-manual-stop.marker}"
SCALE_WINDOW_START="${SCALE_WINDOW_START:-2026-07-06}"
SCALE_WINDOW_END="${SCALE_WINDOW_END:-2026-07-08}"
MIN_DISK_FREE_GB="${MIN_DISK_FREE_GB:-50}"
PROBE_POLL_START_IST="${PROBE_POLL_START_IST:-09:45}"
PROBE_TIMEOUT_MIN="${PROBE_TIMEOUT_MIN:-20}"
EOD_STOP_IST="${EOD_STOP_IST:-15:35}"
CAFFEINATE_UNTIL_IST="${CAFFEINATE_UNTIL_IST:-15:45}"
MONITOR_INTERVAL_SECS="${MONITOR_INTERVAL_SECS:-60}"
# Mid-session disk re-check cadence: every N monitor iterations (default
# 30 × 60s = ~30 min). Edge-latched Telegram — one warn per low-disk episode.
DISK_RECHECK_ITERS="${DISK_RECHECK_ITERS:-30}"
# Missed-run detection: a dated stamp file is written on every successful
# app start; on the next invocation, if the PREVIOUS TRADING day has no
# stamp, the operator gets a "yesterday's run was missed" Telegram.
RUN_STAMP_KEEP="${RUN_STAMP_KEEP:-14}"
QUESTDB_HTTP="${QUESTDB_HTTP:-http://127.0.0.1:9000}"
# FIX 5.1 (adopt-or-kill): ports the app binds — used to detect an already-
# running app (or a foreign port holder) BEFORE launching a new one.
# 3001 = API (config/base.toml [api]); 9091 = metrics exporter
# (config/base.toml metrics_port).
APP_API_PORT="${APP_API_PORT:-3001}"
APP_METRICS_PORT="${APP_METRICS_PORT:-9091}"
HOLIDAYS_FILE="${HOLIDAYS_FILE:-config/base.toml}"
# QuestDB memory: pin 8g for EVERY autopilot-driven compose up so the
# zero-touch max-ladder day gets the SAME limit as a manual `make scale-max`
# (verification finding 2026-07-04: compose default is 6g via the local
# override — a 100k ladder under 6g would gate/roll back earlier than the
# manual path). Env-overridable like every other tunable.
export QDB_MEM_LIMIT="${QDB_MEM_LIMIT:-8g}"

APP_PIDFILE="$AUTOPILOT_DIR/app.pid"
CAFF_PIDFILE="$AUTOPILOT_DIR/caffeinate.pid"
LOCKDIR="$AUTOPILOT_DIR/autopilot.lock.d"

# ── Pure decision functions (unit-tested; no I/O beyond declared file args) ─

# ist_date / ist_hms / ist_secs_of_day — the ONLY clock reads; everything
# downstream takes these as arguments so it stays pure + testable.
ist_date() { TZ=Asia/Kolkata date +%F; }
ist_hms() { TZ=Asia/Kolkata date +%H:%M:%S; }
ist_weekday() { TZ=Asia/Kolkata date +%u; } # 1=Mon .. 7=Sun

# rotated_logs_to_delete <keep_n> <rotated file...> → the files BEYOND the
# newest keep_n, one per line (date-suffixed names sort lexicographically ==
# chronologically). Pure; portable (tail -n +N works on BSD + GNU).
rotated_logs_to_delete() {
  local keep="$1"
  shift
  [ $# -gt 0 ] || return 0
  printf '%s\n' "$@" | sort -r | tail -n +"$((keep + 1))"
}

# hhmm_to_secs "HH:MM[:SS]" → seconds of day (empty on garbage).
hhmm_to_secs() {
  local t="$1" h m s
  IFS=: read -r h m s <<<"$t"
  case "$h$m" in *[!0-9]* | "") return 1 ;; esac
  s="${s:-0}"
  case "$s" in *[!0-9]*) return 1 ;; esac
  if ((10#$h > 23 || 10#$m > 59 || 10#$s > 59)); then return 1; fi
  echo $((10#$h * 3600 + 10#$m * 60 + 10#$s))
}

# secs_until <target HH:MM> <now HH:MM:SS> → seconds to wait (0 if passed).
secs_until() {
  local target now
  target=$(hhmm_to_secs "$1") || {
    echo 0
    return 0
  }
  now=$(hhmm_to_secs "$2") || {
    echo 0
    return 0
  }
  if ((target <= now)); then echo 0; else echo $((target - now)); fi
}

# is_nse_holiday <YYYY-MM-DD> <toml-file> → 0 when the date is listed under
# [[trading.nse_holidays]] (format: date = "YYYY-MM-DD").
is_nse_holiday() {
  local d="$1" f="$2"
  [ -f "$f" ] && grep -qE "^date = \"${d}\"" "$f"
}

# in_scale_window <date> <start> <end> → 0 when start <= date <= end
# (ISO dates compare lexically). A marker file data/local-autopilot/
# scale-window.conf ("START END") overrides the env dates (read by caller).
in_scale_window() {
  local d="$1" s="$2" e="$3"
  [[ "$d" > "$s" || "$d" == "$s" ]] && [[ "$d" < "$e" || "$d" == "$e" ]]
}

# classify_day <date> <weekday 1-7> <holidays-file> <scale-start> <scale-end>
# → weekend | holiday | scale | normal
classify_day() {
  local d="$1" wd="$2" hf="$3" ss="$4" se="$5"
  if ((wd >= 6)); then
    echo weekend
    return 0
  fi
  if is_nse_holiday "$d" "$hf"; then
    echo holiday
    return 0
  fi
  if in_scale_window "$d" "$ss" "$se"; then
    echo scale
    return 0
  fi
  echo normal
}

# parse_probe_verdict — stdin: QuestDB /exec JSON for
#   select reason from groww_scale_audit where outcome='probe_verdict'
#   order by ts desc limit 1
# stdout: the verdict label ("probe_multi_conn_ok", ...) or "" when absent /
# malformed (fail-quiet: the caller treats "" as not-yet).
parse_probe_verdict() {
  python3 -c '
import json, sys
try:
    body = json.load(sys.stdin)
    rows = body.get("dataset") or []
    if rows and rows[0]:
        v = str(rows[0][0])
        if v.startswith("probe_"):
            print(v)
except Exception:
    pass
' 2>/dev/null
}

# next_action_for_verdict <verdict> → scale_max | record_continue |
# continue_normal | none  (the Monday decision tree, pure).
next_action_for_verdict() {
  case "$1" in
  probe_multi_conn_ok) echo scale_max ;;
  probe_per_account_limited) echo record_continue ;;
  probe_inconclusive | probe_smoke_machinery_validated) echo continue_normal ;;
  *) echo none ;;
  esac
}

# manual_stop_active <marker-file> <today> → 0 when a manual stop marker
# exists AND was written today (stale markers from previous days do NOT
# block — "until the next trading day begins").
manual_stop_active() {
  local marker="$1" today="$2" marker_date
  [ -f "$marker" ] || return 1
  marker_date=$(head -1 "$marker" 2>/dev/null | cut -c1-10)
  [ "$marker_date" = "$today" ]
}

# monitor_decision <app_alive 0/1> <manual_stop 0/1> <relaunches so far>
# → idle | manual_standdown | relaunch | alert   (the monitor-loop brain).
# Manual ALWAYS wins: a manual stop is never fought with a relaunch.
monitor_decision() {
  local alive="$1" manual="$2" relaunches="$3"
  if [ "$manual" = "1" ]; then
    echo manual_standdown
    return 0
  fi
  if [ "$alive" = "1" ]; then
    echo idle
    return 0
  fi
  if ((relaunches < 1)); then echo relaunch; else echo alert; fi
}

# disk_low <avail_gb> <min_gb> → 0 when free space is below the floor (pure).
# Non-numeric avail (probe failure) → NOT low (1) — a broken probe must not
# spam warnings; the in-app disk gate is the authoritative guard.
disk_low() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  (($1 < $2))
}

# disk_recheck_due <loop_iter> <every_n> → 0 when this iteration should
# re-probe the disk (pure). every_n <= 0 disables the re-check entirely.
disk_recheck_due() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  case "$2" in '' | *[!0-9]*) return 1 ;; esac
  (($2 > 0)) || return 1
  (($1 % $2 == 0))
}

# mem_limit_bytes <spec like 8g|512m|1024k|1073741824> → bytes (pure).
# Empty/garbage → non-zero return (caller skips the check, never guesses).
mem_limit_bytes() {
  local v n
  v=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
  case "$v" in
  *g)
    n="${v%g}"
    case "$n" in '' | *[!0-9]*) return 1 ;; esac
    echo $((n * 1024 * 1024 * 1024))
    ;;
  *m)
    n="${v%m}"
    case "$n" in '' | *[!0-9]*) return 1 ;; esac
    echo $((n * 1024 * 1024))
    ;;
  *k)
    n="${v%k}"
    case "$n" in '' | *[!0-9]*) return 1 ;; esac
    echo $((n * 1024))
    ;;
  *)
    case "$v" in '' | *[!0-9]*) return 1 ;; esac
    echo "$v"
    ;;
  esac
}

# docker_vm_mem_sufficient <vm_total_bytes> <required_bytes> → 0 when the
# Docker VM has at least the required memory (pure). Either arg empty or
# non-numeric → insufficient (fail-quiet — caller already logged the skip).
docker_vm_mem_sufficient() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  case "$2" in '' | *[!0-9]*) return 1 ;; esac
  (($1 >= $2))
}

# prev_trading_day <YYYY-MM-DD> <holidays-toml> → the most recent trading
# day strictly BEFORE the given date (skips Sat/Sun + [[trading.nse_holidays]]
# entries, same regex as is_nse_holiday). Pure over its arguments; empty
# output on garbage input. Looks back at most 30 calendar days.
prev_trading_day() {
  python3 -c '
import sys, re, datetime as dt
try:
    today = dt.date.fromisoformat(sys.argv[1])
except (ValueError, IndexError):
    sys.exit(0)
hols = set()
try:
    with open(sys.argv[2]) as fh:
        for line in fh:
            m = re.match(r"^date = \"(\d{4}-\d{2}-\d{2})\"", line)
            if m:
                hols.add(m.group(1))
except OSError:
    pass
d = today
for _ in range(30):
    d -= dt.timedelta(days=1)
    if d.weekday() < 5 and d.isoformat() not in hols:
        print(d.isoformat())
        break
' "$1" "$2" 2>/dev/null
}

# questdb_state_degraded <docker-state-string> → 0 when the container state
# says the database is stuck/degraded and ONE `docker restart` is warranted
# (pure over its argument). Input is the output of
#   docker inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' tv-questdb
# e.g. "running healthy", "running unhealthy", "exited (…)", "dead",
# "paused", "running starting". Empty/garbage → NOT degraded (fail-quiet:
# no container info means compose-up will create it fresh — never restart
# blind). "restarting" is NOT degraded — Docker is already restarting it.
questdb_state_degraded() {
  case "$1" in
  *unhealthy*) return 0 ;;
  exited* | dead* | paused*) return 0 ;;
  *) return 1 ;;
  esac
}

# daily_app_log <YYYY-MM-DD> → the ONE human-readable daily log file the app
# and the launcher SHARE (pure). FIX 4 (operator 2026-07-04: literally ONE
# log): all launcher output appends into the SAME data/logs/app.<date>.log.
# Garbage date → a fixed fail-quiet name inside data/logs/ (a write can
# never land outside the logs directory).
daily_app_log() {
  case "$1" in
  [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) echo "data/logs/app.$1.log" ;;
  *) echo "data/logs/app.unknown-date.log" ;;
  esac
}

# refresh_one_file_symlink <daily-log-path> <symlink-path> → keeps the
# operator-facing ONE-file name (data/logs/tickvault.log) pointed at TODAY's
# daily log via `ln -sfn` (atomic replace; macOS Finder follows it fine).
# Relative target (basename) — both live in data/logs/. Failures are quiet:
# the daily file itself is always the truth; the symlink is convenience.
# I/O helper (testable in a tmpdir), safe to define in library mode.
refresh_one_file_symlink() {
  [ -n "${1:-}" ] && [ -n "${2:-}" ] || return 0
  ln -sfn "$(basename "$1")" "$2" 2>/dev/null || true
}

# ── FIX 5 pure helpers (operator LIVE-run bugs, 2026-07-04 20:00 IST) ──────

# first_pid <text> → the first line that is a bare numeric pid (empty when
# none). Pure. Picks one pid out of pgrep output.
first_pid() { printf '%s\n' "${1:-}" | awk '/^[0-9]+$/ {print; exit}'; }

# adopt_or_kill_decision <proc_found 0/1> <healthy 0/1> → adopt|kill|launch.
# Pure. FIX 5.1: a stale app held the metrics port and the fresh launch died
# with "Address already in use (os error 48)".
#   found + healthy   → ADOPT it (write pidfile; manual-wins unchanged)
#   found + unhealthy → KILL it (TERM→wait→KILL), then launch fresh
#   not found         → LAUNCH
adopt_or_kill_decision() {
  if [ "${1:-0}" = "1" ] && [ "${2:-0}" = "1" ]; then
    echo adopt
  elif [ "${1:-0}" = "1" ]; then
    echo kill
  else
    echo launch
  fi
}

# app_launch_verdict <alive 0/1> <waited_secs> <early_window_secs> →
# ok|early_exit|died_later. Pure. FIX 5.1: an app that exits within the
# early window (~5s — port clash, config error) gets the last ~20 log lines
# surfaced with a plain-English message instead of a silent tail of nothing.
app_launch_verdict() {
  if [ "${1:-0}" = "1" ]; then
    echo ok
    return 0
  fi
  case "${2:-}" in '' | *[!0-9]*)
    echo died_later
    return 0
    ;;
  esac
  case "${3:-}" in '' | *[!0-9]*)
    echo died_later
    return 0
    ;;
  esac
  if (($2 <= $3)); then echo early_exit; else echo died_later; fi
}

# clamp_qdb_mem_g <vm_total_bytes> → "<N>g" where N = floor(vm/1GiB) − 1
# (leave ~1 GB for Docker itself), never below 1g. Pure; non-numeric input
# → rc 1 (caller keeps the configured limit). FIX 5.4: a QDB_MEM_LIMIT above
# the Docker VM's total memory launches a database container doomed to be
# killed for memory — clamp for this run instead.
clamp_qdb_mem_g() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  local g=$(($1 / 1073741824 - 1))
  ((g >= 1)) || g=1
  echo "${g}g"
}

# tg_ssm_path <env> <key> → the SSM parameter path the tickvault APP itself
# uses for Telegram (crates/core/src/auth/secret_manager.rs build_ssm_path:
# /tickvault/<env>/telegram/<key>). Pure; empty env defaults to dev.
# FIX 5.2: the launcher's Telegram previously read /dlt/<env>/telegram/* —
# another project's namespace — so every launcher Telegram silently failed.
tg_ssm_path() { echo "/tickvault/${1:-dev}/telegram/$2"; }

# ── FIX 7: auto-raise Docker Desktop VM memory (macOS) — pure helpers ──────

# docker_vm_raise_target <vm_mib> <required_mib> <pref_target_mib> <host_mib>
# The raise DECISION, pure. stdout:
#   "ok"         — the VM already has enough memory (no raise needed)
#   "host_small" — a raise is needed but the host cannot afford it
#                  (target must stay ≤ host RAM / 3)
#   "<N>"        — raise the Docker VM to N MiB (max(pref_target, required))
# rc 1 on any non-numeric input (caller falls back to the FIX-5 clamp).
docker_vm_raise_target() {
  local v
  for v in "${1:-}" "${2:-}" "${3:-}" "${4:-}"; do
    case "$v" in '' | *[!0-9]*) return 1 ;; esac
  done
  if (($1 >= $2)); then
    echo ok
    return 0
  fi
  local target="$3"
  ((target >= $2)) || target="$2"
  if ((target > $4 / 3)); then
    echo host_small
    return 0
  fi
  echo "$target"
}

# docker_settings_mem_key <settings-json-file> → which memory key this Docker
# Desktop settings file uses: "MemoryMiB" (settings-store.json, Docker
# Desktop ≥ 4.30) or "memoryMiB" (legacy settings.json). rc 1 when the file
# is missing, is not valid JSON, or carries neither key. JSON round-trip via
# python3 — never sed on a settings file.
docker_settings_mem_key() {
  [ -f "${1:-}" ] || return 1
  python3 - "$1" <<'PY'
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
except Exception:
    sys.exit(1)
if not isinstance(d, dict):
    sys.exit(1)
for k in ("MemoryMiB", "memoryMiB"):
    if k in d:
        print(k)
        sys.exit(0)
sys.exit(1)
PY
}

# docker_settings_set_mem <settings-json-file> <key> <mib> — set the memory
# key to <mib> via a python3 JSON round-trip (temp file + atomic replace;
# every other setting preserved). rc 1 on missing file / bad JSON / absent
# key / non-numeric mib — the FILE IS UNTOUCHED on every failure path.
docker_settings_set_mem() {
  [ -f "${1:-}" ] || return 1
  case "${3:-}" in '' | *[!0-9]*) return 1 ;; esac
  python3 - "$1" "$2" "$3" <<'PY'
import json, os, sys
path, key, mib = sys.argv[1], sys.argv[2], int(sys.argv[3])
try:
    with open(path) as f:
        d = json.load(f)
except Exception:
    sys.exit(1)
if not isinstance(d, dict) or key not in d:
    sys.exit(1)
d[key] = mib
tmp = path + ".tickvault-tmp"
try:
    with open(tmp, "w") as f:
        json.dump(d, f, indent=2)
    os.replace(tmp, path)
except Exception:
    try:
        os.unlink(tmp)
    except OSError:
        pass
    sys.exit(1)
PY
}

# ── End of pure functions. Library mode stops here. ────────────────────────
if [[ "${AUTOPILOT_LIB:-0}" == "1" ]]; then
  return 0 2>/dev/null || exit 0
fi

set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p "$AUTOPILOT_DIR" data/logs

TODAY="$(ist_date)"
# ONE HUMAN LOG FILE (FIX 4, operator 2026-07-04 escalated: literally ONE
# file, no date-picking). EVERYTHING human-readable — the launcher's own
# lines (prefixed [launcher]), the cargo build output, AND the app's
# stdout/stderr — appends into the SAME daily rolling file the app uses:
#   data/logs/app.<YYYY-MM-DD>.log        (one file per day)
#   data/logs/tickvault.log               (symlink → always TODAY's file)
# The filename is recomputed on EVERY write (current_log), so long-running
# loops roll to the new day's file at IST midnight, and the tickvault.log
# symlink is re-pointed on every launcher write (ln -sfn — atomic, cheap).
# Machine-format sinks (errors.jsonl.*, errors.log, the app's hourly
# app.YYYY-MM-DD-HH tracing chunks) are RULE-LOCKED robot copies
# (observability-architecture.md) and untouched — this consolidation is the
# HUMAN file only. Honest notes: (a) raw tool output (cargo/docker) appends
# unprefixed between [launcher] marker lines; (b) the app PROCESS keeps the
# file handle it was launched with, so a run crossing IST midnight keeps
# writing its launch-day file until the daily 15:35 stop → next-morning
# start rolls it — in practice each trading day's file carries that day's
# run; (c) launchd itself cannot date-template paths, so the plist points
# at a tiny fixed shim (launchd-boot.log) that only ever sees pre-script
# crashes — every normal line lands in the daily file via this script.
current_log() {
  local f
  f="$(daily_app_log "$(ist_date)")"
  refresh_one_file_symlink "$f" "$ONE_FILE_LINK"
  echo "$f"
}

# Legacy sweep (FIX 4): the pre-2026-07-04 separate launcher log
# (data/logs/autopilot/autopilot.log + its dated rotations) is retired —
# remove it so the operator can never open a stale second file.
rm -rf data/logs/autopilot 2>/dev/null || true

# log(): append to the ONE daily file with a greppable [launcher] prefix;
# echo to the terminal only when interactive.
log() {
  local line="[$(ist_hms) IST] [launcher] $*"
  echo "$line" >>"$(current_log)"
  if [ -t 1 ]; then echo "$line"; fi
}

# tg_init — FIX 5.2: fetch the Telegram credentials ONCE per run from the
# SAME SSM parameters the app itself uses (/tickvault/<env>/telegram/*, per
# secret_manager.rs build_ssm_path — the launcher previously pointed at a
# /dlt/... path from another project, so every launcher Telegram failed).
# Cached in-memory (exported env — the same names bootstrap.sh exports, so a
# bootstrap-launched run skips the fetch entirely). Missing param / offline
# Mac → log ONCE, disable for the run, never spam per message.
TG_INIT_DONE=0
TG_DISABLED=0
tg_init() {
  if ((TG_INIT_DONE)); then return 0; fi
  TG_INIT_DONE=1
  if [ -n "${TV_TELEGRAM_BOT_TOKEN:-}" ] && [ -n "${TV_TELEGRAM_CHAT_ID:-}" ]; then
    return 0 # already cached (e.g. bootstrap.sh exported them)
  fi
  local env="${ENVIRONMENT:-dev}" tok="" chat=""
  if command -v aws >/dev/null 2>&1; then
    tok=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
      --name "$(tg_ssm_path "$env" bot-token)" \
      --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
    chat=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
      --name "$(tg_ssm_path "$env" chat-id)" \
      --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
  fi
  if [ -n "$tok" ] && [ "$tok" != "None" ] && [ -n "$chat" ] && [ "$chat" != "None" ]; then
    export TV_TELEGRAM_BOT_TOKEN="$tok" TV_TELEGRAM_CHAT_ID="$chat"
  else
    TG_DISABLED=1
    log "Telegram unavailable ($(tg_ssm_path "$env" bot-token) unreadable — no internet or missing parameter). Messages will appear in this log only for the rest of this run."
  fi
}

tg() { # best-effort Telegram; the log always carries the message
  log "TELEGRAM: $*"
  tg_init
  if ((TG_DISABLED)); then return 0; fi
  bash scripts/notify-telegram.sh "💻 LOCAL autopilot — $*" >>"$(current_log)" 2>&1 || true
}

# export_compose_creds — FIX 5.3: the launcher's own `docker compose`
# invocations previously ran WITHOUT the SSM-fetched credentials that
# bootstrap.sh exports, so compose failed interpolating
# TV_QUESTDB_PG_PASSWORD whenever the launcher had to (re)create the
# database container. Fetch ONCE per run, same SSM paths bootstrap.sh /
# ensure-questdb.sh use; already-exported env (a bootstrap-launched run)
# skips the fetch. Failure logs once and continues — an EXISTING container
# needs no creds to start.
COMPOSE_CREDS_DONE=0
export_compose_creds() {
  if ((COMPOSE_CREDS_DONE)); then return 0; fi
  COMPOSE_CREDS_DONE=1
  if [ -n "${TV_QUESTDB_PG_USER:-}" ] && [ -n "${TV_QUESTDB_PG_PASSWORD:-}" ]; then
    return 0
  fi
  local env="${ENVIRONMENT:-dev}" u="" p=""
  if command -v aws >/dev/null 2>&1; then
    u=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
      --name "/tickvault/${env}/questdb/pg-user" \
      --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
    p=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
      --name "/tickvault/${env}/questdb/pg-password" \
      --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
  fi
  if [ -n "$u" ] && [ "$u" != "None" ] && [ -n "$p" ] && [ "$p" != "None" ]; then
    export TV_QUESTDB_PG_USER="$u" TV_QUESTDB_PG_PASSWORD="$p"
  else
    log "QuestDB credentials unreadable from SSM (/tickvault/${env}/questdb/*) — the database container can still START if it already exists, but compose cannot (re)create it until the Mac is online"
  fi
}

# ── Service primitives (shared by autopilot + manual start/stop) ───────────

app_alive() {
  [ -f "$APP_PIDFILE" ] && kill -0 "$(cat "$APP_PIDFILE")" 2>/dev/null
}

docker_alive() { docker info >/dev/null 2>&1; }

ensure_docker() {
  if ! command -v docker >/dev/null 2>&1; then
    tg "🆘 Docker is NOT INSTALLED on this Mac — cannot start. Install Docker Desktop, then double-click Start TickVault."
    return 1
  fi
  if docker_alive; then return 0; fi
  log "Docker daemon not running — attempting to launch Docker.app"
  if [ "$(uname -s)" = "Darwin" ]; then open -a Docker >>"$(current_log)" 2>&1 || true; fi
  local i
  for i in $(seq 1 24); do
    sleep 5
    if docker_alive; then
      log "Docker daemon is up (waited $((i * 5))s)"
      return 0
    fi
  done
  tg "🆘 Docker daemon did not come up within 120s — start Docker Desktop manually, then double-click Start TickVault."
  return 1
}

# Live container state string for tv-questdb (empty when no container).
questdb_container_state() {
  docker inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' tv-questdb 2>/dev/null || true
}

ensure_questdb() {
  export_compose_creds # FIX 5.3 — compose interpolates TV_QUESTDB_PG_* env
  docker compose -f deploy/docker/docker-compose.yml \
    -f deploy/docker/docker-compose.local.yml up -d tv-questdb >>"$(current_log)" 2>&1
  # Self-heal (2026-07-04): a degraded container (unhealthy/exited/dead/
  # paused — e.g. after a hard Mac sleep or an out-of-memory kill) gets ONE
  # automatic `docker restart` if the database has not answered after ~30s.
  # Zero operator action — Stop→Start (or just Start) recovers it.
  local i restarted=0 state
  for i in $(seq 1 24); do
    if curl -sf -G "$QUESTDB_HTTP/exec" --data-urlencode "query=select 1" >/dev/null 2>&1; then
      log "QuestDB HTTP ready"
      return 0
    fi
    state="$(questdb_container_state)"
    if ((restarted == 0 && i >= 6)) && questdb_state_degraded "$state"; then
      log "QuestDB container degraded ('$state') — one automatic docker restart"
      tg "⚠️ The database container was stuck — restarting it automatically. No action needed."
      docker restart tv-questdb >>"$(current_log)" 2>&1 || true
      restarted=1
    fi
    sleep 5
  done
  tg "🆘 The database did not start within 2 minutes. Double-click Stop TickVault, wait 10 seconds, then double-click Start TickVault. If it still fails, restart the Mac and double-click Start TickVault again."
  return 1
}

disk_free_ok() {
  local avail_kb
  avail_kb=$(df -Pk . | awk 'NR==2 {print $4}')
  if ((avail_kb / 1024 / 1024 < MIN_DISK_FREE_GB)); then
    tg "⚠️ Low disk: $((avail_kb / 1024 / 1024)) GB free (< ${MIN_DISK_FREE_GB} GB). Free space before the next run — the in-app disk gate will also auto-roll-back the fleet."
    return 1
  fi
  return 0
}

start_caffeinate() {
  command -v caffeinate >/dev/null 2>&1 || return 0
  if [ -f "$CAFF_PIDFILE" ] && kill -0 "$(cat "$CAFF_PIDFILE")" 2>/dev/null; then
    return 0
  fi
  local secs
  secs=$(secs_until "$CAFFEINATE_UNTIL_IST" "$(ist_hms)")
  if ((secs > 0)); then
    caffeinate -dims -t "$secs" &
    echo $! >"$CAFF_PIDFILE"
    log "caffeinate armed for ${secs}s (Mac stays awake until $CAFFEINATE_UNTIL_IST IST; lid must stay OPEN)"
  fi
}

stop_caffeinate() {
  if [ -f "$CAFF_PIDFILE" ]; then
    kill "$(cat "$CAFF_PIDFILE")" 2>/dev/null || true
    rm -f "$CAFF_PIDFILE"
  fi
}

# find_running_app — FIX 5.1: detect an already-running app by PROCESS, not
# just the pidfile (the pidfile can be missing/stale after a crashed
# launcher — the live 2026-07-04 failure: a healthy app was running, the
# blind relaunch died on "Address already in use").
find_running_app() {
  first_pid "$(pgrep -f 'target/release/tickvault' 2>/dev/null || true)"
}

app_port_busy() { # $1 = port → rc 0 when SOMETHING is listening on it
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
  else
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && {
      exec 3>&- 3<&- || true
      return 0
    }
    return 1
  fi
}

app_health_ok() { # the /health endpoint is deliberately public
  curl -sf -m 3 "http://127.0.0.1:${APP_API_PORT}/health" >/dev/null 2>&1
}

start_app() { # $1 = label for the log
  if app_alive; then
    log "app already running (pid $(cat "$APP_PIDFILE")) — not double-starting"
    write_run_stamp
    return 0
  fi
  # FIX 5.1 — adopt-or-kill BEFORE launching: an existing app process
  # (found by name + confirmed by ports/health) is ADOPTED when healthy,
  # or stopped (polite then forced) when zombie/unhealthy, so "Run
  # tickvault" NEVER dies on a port clash.
  local existing decision healthy=0
  existing="$(find_running_app)"
  if [ -n "$existing" ]; then
    if app_health_ok; then healthy=1; fi
    decision="$(adopt_or_kill_decision 1 "$healthy")"
  else
    if app_port_busy "$APP_METRICS_PORT" || app_port_busy "$APP_API_PORT"; then
      tg "🆘 Another program is holding the app's network port (${APP_METRICS_PORT} or ${APP_API_PORT}) and it is not tickvault — quit that program, then double-click Start TickVault."
      return 1
    fi
    decision=launch
  fi
  case "$decision" in
  adopt)
    echo "$existing" >"$APP_PIDFILE"
    log "adopted running app pid $existing (pidfile was missing/stale — no relaunch needed)"
    write_run_stamp
    return 0
    ;;
  kill)
    log "found a stale/unhealthy app process (pid $existing) — stopping it before a fresh launch"
    kill "$existing" 2>/dev/null || true
    local w
    for w in $(seq 1 10); do
      kill -0 "$existing" 2>/dev/null || break
      sleep 1
    done
    if kill -0 "$existing" 2>/dev/null; then
      log "old app pid $existing ignored the polite stop — forcing"
      kill -9 "$existing" 2>/dev/null || true
      sleep 1
    fi
    ;;
  esac
  log "building release binary (first build of the day can take minutes)..."
  if ! cargo build --release >>"$(current_log)" 2>&1; then
    tg "🆘 Build FAILED — the app cannot start. Last lines: $(tail -5 "$(current_log)" | tr '\n' ' | ')"
    return 1
  fi
  nohup ./target/release/tickvault >>"$(current_log)" 2>&1 &
  echo $! >"$APP_PIDFILE"
  log "app launched ($1) — pid $(cat "$APP_PIDFILE"), log $ONE_FILE_LINK → $(current_log)"
  # FIX 5.1 — early-exit detection: an app that dies within ~5s (port clash,
  # config error) gets its last log lines SHOWN with a plain-English message
  # instead of the old silent 60s wait.
  sleep 5
  local verdict
  verdict="$(app_launch_verdict "$(app_alive && echo 1 || echo 0)" 5 5)"
  if [ "$verdict" = "early_exit" ]; then
    log "app exited within 5s of launch ($1) — the last 20 log lines follow"
    if [ -t 1 ]; then
      echo "---- why the app stopped (last 20 log lines) ----"
      tail -20 "$(current_log)"
      echo "-------------------------------------------------"
    fi
    tg "🆘 App stopped immediately after launch ($1). Most recent messages: $(tail -8 "$(current_log)" | tr '\n' ' | '). Double-click Stop TickVault, wait 10 seconds, then Start TickVault."
    return 1
  fi
  sleep 55
  if ! app_alive; then
    tg "🆘 App DIED within 60s of launch ($1). Log tail: $(tail -8 "$(current_log)" | tr '\n' ' | ')"
    return 1
  fi
  write_run_stamp
  return 0
}

# Missed-run detection support: a dated stamp file per SUCCESSFUL app start
# (auto or manual — both mean "the app ran today"). Old stamps pruned beyond
# RUN_STAMP_KEEP (date-suffixed names sort chronologically).
write_run_stamp() {
  printf '%s %s\n' "$TODAY" "$(ist_hms)" >"$AUTOPILOT_DIR/run-stamp.$TODAY"
  local stamps=("$AUTOPILOT_DIR"/run-stamp.*)
  if [ -e "${stamps[0]}" ]; then
    while IFS= read -r doomed; do
      if [ -n "$doomed" ]; then rm -f "$doomed"; fi
    done < <(rotated_logs_to_delete "$RUN_STAMP_KEEP" "${stamps[@]}")
  fi
}

# check_missed_previous_run — if the previous TRADING day has no run stamp
# (Mac off/asleep, launchd never fired, or the run died before start), warn
# the operator ONCE. Suppressed on a fresh install (no stamps at all yet —
# nothing to compare against, warning would be noise).
check_missed_previous_run() {
  local prev
  prev=$(prev_trading_day "$TODAY" "$HOLIDAYS_FILE")
  [ -n "$prev" ] || return 0
  if ! ls "$AUTOPILOT_DIR"/run-stamp.* >/dev/null 2>&1; then
    log "missed-run check: no run stamps yet (first install) — skipping"
    return 0
  fi
  if [ ! -f "$AUTOPILOT_DIR/run-stamp.$prev" ]; then
    tg "⚠️ Yesterday's run was missed ($prev had no capture — Mac likely off or asleep at 8:55 AM). Today's run is starting now; keep the Mac on with the lid open on trading mornings."
  else
    log "missed-run check: $prev has a run stamp — OK"
  fi
}

# ── FIX 7: auto-raise Docker Desktop VM memory (macOS, once per run) ───────
# auto_raise_docker_vm_memory <vm_bytes> <limit_bytes> — rc 0 when the raise
# succeeded AND the restarted Docker VM covers the QuestDB pin; rc 1 on any
# other outcome (caller falls back to the FIX-5 clamp + manual advice).
# Never bricks Docker: settings file is backed up first and restored on ANY
# failure (missing file, bad JSON, restart timeout). Skipped on Linux/CI.
DOCKER_VM_RAISE_ATTEMPTED=0
auto_raise_docker_vm_memory() {
  [ "$(uname -s)" = "Darwin" ] || return 1
  [ -z "${CI:-}" ] || return 1
  [ "$DOCKER_VM_RAISE_ATTEMPTED" = "0" ] || return 1
  DOCKER_VM_RAISE_ATTEMPTED=1
  local vm_mib=$(($1 / 1048576))
  # Required = QuestDB pin + 2 GB headroom (Docker itself + the app's containers).
  local required_mib=$(($2 / 1048576 + 2048))
  local pref="${TV_DOCKER_VM_MEM_MIB:-10240}"
  case "$pref" in '' | *[!0-9]*) pref=10240 ;; esac
  local host_bytes host_mib
  host_bytes=$(sysctl -n hw.memsize 2>/dev/null || true)
  case "$host_bytes" in '' | *[!0-9]*)
    log "host RAM probe unavailable — skipping Docker VM auto-raise"
    return 1
    ;;
  esac
  host_mib=$((host_bytes / 1048576))
  local target
  if ! target=$(docker_vm_raise_target "$vm_mib" "$required_mib" "$pref" "$host_mib"); then
    return 1
  fi
  case "$target" in
  ok) return 0 ;;
  host_small)
    log "Docker VM auto-raise skipped — this Mac's RAM (${host_mib} MiB) is too small to give Docker ${required_mib} MiB"
    return 1
    ;;
  esac
  # Locate the settings file + its memory key (Docker Desktop ≥ 4.30 first,
  # then the legacy file).
  local gc="$HOME/Library/Group Containers/group.com.docker"
  local file="" key="" cand
  for cand in "$gc/settings-store.json" "$gc/settings.json"; do
    if key=$(docker_settings_mem_key "$cand"); then
      file="$cand"
      break
    fi
  done
  if [ -z "$file" ]; then
    log "Docker Desktop settings file not found under '$gc' — cannot auto-raise VM memory"
    return 1
  fi
  local bak="${file}.tickvault-bak"
  cp "$file" "$bak" 2>/dev/null || return 1
  if ! docker_settings_set_mem "$file" "$key" "$target"; then
    cp "$bak" "$file" 2>/dev/null || true
    log "Docker settings edit failed — backup restored; falling back to the memory clamp"
    return 1
  fi
  log "raising Docker Desktop memory ${vm_mib} MiB → ${target} MiB ($(basename "$file") key ${key}) — restarting Docker Desktop now"
  osascript -e 'quit app "Docker"' >/dev/null 2>&1 || true
  local i
  for i in $(seq 1 10); do # wait for the daemon to go down (~30s max)
    docker info >/dev/null 2>&1 || break
    sleep 3
  done
  open -a Docker 2>/dev/null || true
  local ready=0
  for i in $(seq 1 40); do # wait for the daemon to come back (~120s max)
    if docker info >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 3
  done
  if [ "$ready" != "1" ]; then
    cp "$bak" "$file" 2>/dev/null || true
    open -a Docker 2>/dev/null || true
    log "Docker did not come back within ~120s after the memory raise — old setting restored; falling back to the memory clamp"
    tg "⚠️ Tried to raise Docker's memory automatically but Docker did not restart in time. The old setting was put back and the database will run with a smaller memory budget this run."
    return 1
  fi
  local new_vm
  new_vm=$(docker info --format '{{.MemTotal}}' 2>/dev/null || true)
  case "$new_vm" in '' | *[!0-9]*) return 1 ;; esac
  local new_mib=$((new_vm / 1048576))
  if [ "$new_vm" -ge "$2" ]; then
    log "Docker VM memory raised: ${vm_mib} MiB → ${new_mib} MiB (target ${target} MiB) — database keeps its full ${QDB_MEM_LIMIT} budget"
    tg "✅ Docker's memory was raised automatically (${vm_mib} → ${new_mib} MiB) so the database gets its full budget. No action needed."
    return 0
  fi
  log "Docker restarted but its VM still has ${new_mib} MiB (< required) — falling back to the memory clamp"
  return 1
}

# check_docker_vm_memory — warn (log + Telegram, non-blocking) when the
# Docker VM has less memory than the QuestDB pin (QDB_MEM_LIMIT). On Docker
# Desktop the VM ceiling silently caps the container limit — QuestDB would
# be OOM-killed under load instead of using its pinned budget.
check_docker_vm_memory() {
  local vm_bytes limit_bytes
  vm_bytes=$(docker info --format '{{.MemTotal}}' 2>/dev/null || true)
  case "$vm_bytes" in '' | *[!0-9]*)
    log "docker VM memory probe unavailable — skipping check"
    return 0
    ;;
  esac
  if ! limit_bytes=$(mem_limit_bytes "$QDB_MEM_LIMIT"); then
    log "QDB_MEM_LIMIT '$QDB_MEM_LIMIT' unparsable — skipping VM memory check"
    return 0
  fi
  if ! docker_vm_mem_sufficient "$vm_bytes" "$limit_bytes"; then
    local vm_gb=$((vm_bytes / 1024 / 1024 / 1024))
    log "Docker VM memory ${vm_bytes} bytes < QDB_MEM_LIMIT ${QDB_MEM_LIMIT}"
    # FIX 7: first try to RAISE Docker Desktop's VM memory automatically
    # (macOS only, once per run, backup + restore on any failure). Success
    # means the database keeps its full pinned budget — no clamp needed.
    if auto_raise_docker_vm_memory "$vm_bytes" "$limit_bytes"; then
      log "docker VM memory sufficient after auto-raise — keeping QDB_MEM_LIMIT ${QDB_MEM_LIMIT}"
      return 0
    fi
    # FIX 5.4: do NOT launch a database container doomed to be killed for
    # memory — clamp the limit for THIS RUN to (VM memory − 1 GB, min 1g).
    local clamped
    if clamped=$(clamp_qdb_mem_g "$vm_bytes"); then
      log "auto-clamping QDB_MEM_LIMIT ${QDB_MEM_LIMIT} → ${clamped} for this run"
      tg "⚠️ Docker Desktop only has ${vm_gb} GB memory but the database wants ${QDB_MEM_LIMIT}. Using ${clamped} for this run so the database cannot crash from memory. For full speed: open Docker Desktop → Settings → Resources → Memory, raise it to 10 GB, click Apply & Restart, then double-click Start TickVault."
      export QDB_MEM_LIMIT="$clamped"
    else
      tg "⚠️ Docker Desktop only has ${vm_gb} GB memory but the database needs ${QDB_MEM_LIMIT}. Open Docker Desktop → Settings → Resources → Memory and raise it to 10 GB, then restart. Running anyway — but the database may crash under load until this is fixed."
    fi
  else
    log "docker VM memory OK (${vm_bytes} bytes >= QDB_MEM_LIMIT ${QDB_MEM_LIMIT})"
  fi
  return 0
}

stop_app() {
  if ! app_alive; then
    log "app not running — nothing to stop"
    rm -f "$APP_PIDFILE"
    return 0
  fi
  local pid
  pid=$(cat "$APP_PIDFILE")
  log "stopping app (pid $pid) gracefully..."
  kill -TERM "$pid" 2>/dev/null || true
  local i
  for i in $(seq 1 12); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 5
  done
  if kill -0 "$pid" 2>/dev/null; then
    log "app did not exit in 60s — force kill"
    kill -KILL "$pid" 2>/dev/null || true
  fi
  rm -f "$APP_PIDFILE"
}

apply_overlay() { # $1 = groww-scale-test.sh mode (probe|max|...) or "none"
  if [ "$1" = "none" ]; then
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1
  else
    DRY_RUN=1 bash scripts/groww-scale-test.sh "$1" >>"$(current_log)" 2>&1
  fi
  log "config overlay: $1"
}

# ── Probe verdict polling ───────────────────────────────────────────────────

fetch_probe_verdict() {
  curl -sf -G "$QUESTDB_HTTP/exec" --data-urlencode \
    "query=select reason from groww_scale_audit where outcome = 'probe_verdict' and trading_date_ist = '$TODAY' order by ts desc limit 1" \
    2>/dev/null | parse_probe_verdict
}

# ── Subcommand: manual start ────────────────────────────────────────────────

cmd_start() {
  log "== MANUAL START (manual always wins — clearing manual-stop marker) =="
  rm -f "$MANUAL_STOP_MARKER"
  ensure_docker || return 1
  check_docker_vm_memory
  ensure_questdb || return 1
  disk_free_ok || true # warn-only on manual start — operator's call
  start_caffeinate
  start_app "manual start" || return 1
  tg "🚀 Manual start complete — app running, feed capture live. Autopilot (if scheduled) will adopt this app, not double-start."
}

# ── Subcommand: manual stop ─────────────────────────────────────────────────

cmd_stop() {
  log "== MANUAL STOP (writing manual-stop marker — autopilot stands down today) =="
  stop_app
  bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
  stop_caffeinate
  # Leave QuestDB up — but if the container is degraded, restart it now so
  # the next Start (and any manual queries) find a healthy database.
  if docker_alive; then
    local qdb_state
    qdb_state="$(questdb_container_state)"
    if questdb_state_degraded "$qdb_state"; then
      log "QuestDB container degraded at stop ('$qdb_state') — restarting it so it is healthy for the next Start"
      docker restart tv-questdb >>"$(current_log)" 2>&1 || true
    fi
  fi
  printf '%s %s manual stop\n' "$TODAY" "$(ist_hms)" >"$MANUAL_STOP_MARKER"
  tg "🔔 Manual stop done — app stopped, overlay cleaned, QuestDB left running. Autopilot will NOT auto-restart today. Double-click Start TickVault to resume."
}

# ── Subcommand: dev (the ONE-BUTTON IntelliJ "Run tickvault") ───────────────
# Operator 2026-07-04: "just provide everything in the fucking run tickvault
# thats it and it should have everything". Full chain from a cold Mac:
#   ensure-ready (dev tools + docker auto-setup, idempotent, fast-path <1s)
#   → docker compose up with the local override (QDB_MEM_LIMIT=8g exported in
#     the tunables block above) → wait for QuestDB to answer → build + boot
#     the app (idempotent — adopts an already-running app via the pidfile)
#   → tail the app log in the FOREGROUND so the IntelliJ run window shows it.
# Stopping the tail (red square / Ctrl+C) does NOT stop the app — use the
# "Stop tickvault" run config (make local-stop) for a graceful stop.

cmd_dev() {
  log "== ONE-BUTTON DEV RUN (ensure-ready → docker → questdb → app → tail) =="
  if [ -x scripts/ensure-ready.sh ]; then
    bash scripts/ensure-ready.sh || {
      log "ensure-ready failed — cannot continue"
      return 1
    }
  else
    log "scripts/ensure-ready.sh missing/not executable — skipping (docker + questdb still ensured below)"
  fi
  cmd_start || return 1
  # ONE file (FIX 4): launcher + app all write today's daily file; tail it.
  local app_log
  app_log="$(current_log)"
  echo "=== tickvault is RUNNING — tailing $app_log (fixed name: $ONE_FILE_LINK) ==="
  echo "=== stopping this tail does NOT stop the app; use the 'Stop tickvault' run config ==="
  exec tail -f "$app_log"
}

# ── Subcommand: status ──────────────────────────────────────────────────────

cmd_status() {
  echo "branch : $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
  echo "date   : $TODAY ($(classify_day "$TODAY" "$(ist_weekday)" "$HOLIDAYS_FILE" "$(scale_window_start)" "$(scale_window_end)") day)"
  if app_alive; then echo "app    : RUNNING (pid $(cat "$APP_PIDFILE"))"; else echo "app    : stopped"; fi
  if docker_alive; then echo "docker : up"; else echo "docker : DOWN"; fi
  if manual_stop_active "$MANUAL_STOP_MARKER" "$TODAY"; then
    echo "manual : STOP marker active ($(head -1 "$MANUAL_STOP_MARKER")) — autopilot stands down"
  else
    echo "manual : no active stop marker"
  fi
  echo "logs   : $ONE_FILE_LINK → $(current_log) (ONE file — app + launcher + build output)"
}

# scale-window override file: "START END" (ISO dates) in
# data/local-autopilot/scale-window.conf beats the env defaults.
scale_window_start() {
  local f="$AUTOPILOT_DIR/scale-window.conf"
  if [ -f "$f" ]; then awk '{print $1}' "$f"; else echo "$SCALE_WINDOW_START"; fi
}
scale_window_end() {
  local f="$AUTOPILOT_DIR/scale-window.conf"
  if [ -f "$f" ]; then awk '{print $2}' "$f"; else echo "$SCALE_WINDOW_END"; fi
}

# ── Subcommand: the unattended trading-day run ──────────────────────────────

preflight_git() {
  local branch
  branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
  if [ "$branch" != "local-runtime" ]; then
    log "on branch '$branch' — attempting checkout of local-runtime"
    if ! git checkout local-runtime >>"$(current_log)" 2>&1; then
      tg "⚠️ Could not switch to the local-runtime branch (uncommitted changes?). Running the EXISTING checkout '$branch' — code may be stale."
      return 0
    fi
  fi
  if git fetch origin >>"$(current_log)" 2>&1; then
    if ! git merge --ff-only origin/local-runtime >>"$(current_log)" 2>&1; then
      tg "⚠️ local-runtime has diverged from origin (conflict?) — running the EXISTING local code. Resolve with: git merge origin/local-runtime"
    fi
  else
    log "git fetch failed (offline?) — running existing code"
  fi
}

wait_until_ist() { # $1 = "HH:MM"; returns immediately if already past
  local secs
  secs=$(secs_until "$1" "$(ist_hms)")
  if ((secs > 0)); then
    log "waiting ${secs}s until $1 IST..."
    sleep "$secs"
  fi
}

eod_summary() {
  local mode="$1" summary="data/groww-scale/summary-$TODAY.tsv" digest="" errors
  errors=$(grep -c "ERROR" "$(current_log)" 2>/dev/null || true)
  if [ -f "$summary" ]; then
    digest=$'\nStage table (outcome | conns | proof | capturing | bytes | cpu | mem):\n'
    digest+=$(awk -F'\t' 'NR>1 {printf "%s | %s | %s | %s | %s | %s%% | %s%%\n", $2, $3, $6, $7, $8, $9, $10}' "$summary" | tail -12)
  fi
  tg "🔔 End of day ($mode day). App stopped 15:35 IST, overlay cleaned. ERROR lines in app log: ${errors:-0}.${digest}"
}

monitor_until_eod() {
  local relaunches=0 decision alive manual iter=0 disk_warned=0 avail_gb
  while :; do
    if (($(secs_until "$EOD_STOP_IST" "$(ist_hms)") == 0)); then break; fi
    sleep "$MONITOR_INTERVAL_SECS"
    iter=$((iter + 1))
    alive=0
    app_alive && alive=1
    manual=0
    manual_stop_active "$MANUAL_STOP_MARKER" "$TODAY" && manual=1
    # Periodic disk re-check (2026-07-04 resilience fix): the boot-time
    # disk_free_ok gate cannot see a disk that fills MID-SESSION (ticks +
    # spill + logs all grow). Re-probe every DISK_RECHECK_ITERS iterations;
    # edge-latched so one low-disk episode = one Telegram, re-armed on
    # recovery. Warn-only — the in-app disk gate owns any halting decision.
    if disk_recheck_due "$iter" "$DISK_RECHECK_ITERS"; then
      avail_gb=$(df -Pk . 2>/dev/null | awk 'NR==2 {print int($4/1024/1024)}')
      if disk_low "$avail_gb" "$MIN_DISK_FREE_GB"; then
        if [ "$disk_warned" = "0" ]; then
          tg "⚠️ Low disk MID-SESSION: ${avail_gb} GB free (below the ${MIN_DISK_FREE_GB} GB floor). Free up space soon — the capture keeps running but the safety systems will start protecting themselves if the disk keeps filling."
          disk_warned=1
        fi
        log "disk re-check: LOW (${avail_gb} GB free < ${MIN_DISK_FREE_GB} GB)"
      else
        if [ "$disk_warned" = "1" ]; then
          log "disk re-check: recovered (${avail_gb:-?} GB free) — warn latch re-armed"
        fi
        disk_warned=0
      fi
    fi
    decision=$(monitor_decision "$alive" "$manual" "$relaunches")
    case "$decision" in
    idle) : ;;
    manual_standdown)
      log "manual stop marker present — autopilot stands down (no relaunch, no fight)"
      ;;
    relaunch)
      relaunches=$((relaunches + 1))
      tg "⚠️ App died mid-session — relaunching once (attempt $relaunches). Log tail: $(tail -5 "$(current_log)" | tr '\n' ' | ')"
      sleep 30
      start_app "auto-relaunch" || true
      ;;
    alert)
      tg "🆘 App died AGAIN after the one relaunch — NOT retrying (avoid crash-looping the feed). Log tail: $(tail -8 "$(current_log)" | tr '\n' ' | '). Double-click Start TickVault after investigating."
      relaunches=$((relaunches + 1)) # alert once, then idle on this state
      ;;
    esac
    # Docker watchdog (independent of the app): one self-heal try + alert.
    if ! docker_alive && [ "$manual" = "0" ]; then
      tg "⚠️ Docker daemon went away mid-session — attempting restart"
      ensure_docker && ensure_questdb || true
    fi
  done
}

cmd_run() {
  # Duplicate-instance lock (mkdir is atomic; stale lock from a dead pid is
  # reclaimed).
  if ! mkdir "$LOCKDIR" 2>/dev/null; then
    local old
    old=$(cat "$LOCKDIR/pid" 2>/dev/null || echo "")
    if [ -n "$old" ] && kill -0 "$old" 2>/dev/null; then
      log "another autopilot instance (pid $old) is running — exiting"
      exit 0
    fi
    log "reclaiming stale lock (pid ${old:-?} is gone)"
  fi
  echo $$ >"$LOCKDIR/pid"
  trap 'rm -rf "$LOCKDIR"' EXIT

  log "===== LOCAL AUTOPILOT run starting ====="
  # Missed-run detection (2026-07-04 resilience fix): if the previous
  # TRADING day has no run stamp, the Mac was likely off/asleep and a whole
  # session's capture was silently lost — tell the operator.
  check_missed_previous_run
  local day
  day=$(classify_day "$TODAY" "$(ist_weekday)" "$HOLIDAYS_FILE" "$(scale_window_start)" "$(scale_window_end)")
  log "day classification: $day (scale window $(scale_window_start)..$(scale_window_end))"
  if [ "$day" = "weekend" ] || [ "$day" = "holiday" ]; then
    log "market closed today — quiet no-op"
    exit 0
  fi
  if manual_stop_active "$MANUAL_STOP_MARKER" "$TODAY"; then
    log "manual stop marker already set for today — autopilot stands down entirely"
    exit 0
  fi
  # Stale marker from a previous day: a new trading day clears it.
  rm -f "$MANUAL_STOP_MARKER"

  # Guard against firing outside the day window (launchd anomalies, manual
  # invocation at night): after EOD → no-op.
  if (($(secs_until "$EOD_STOP_IST" "$(ist_hms)") == 0)); then
    log "already past $EOD_STOP_IST IST — nothing to do today"
    exit 0
  fi

  preflight_git
  ensure_docker || exit 1
  check_docker_vm_memory
  disk_free_ok || exit 1
  ensure_questdb || exit 1
  start_caffeinate

  if [ "$day" = "scale" ]; then
    apply_overlay probe
    start_app "scale day — probe overlay" || exit 1
    tg "🚀 Scale-lab day: app booted with the 2×600 cap-probe. Verdict expected between $PROBE_POLL_START_IST and ~10:05 IST — I will read it and act automatically."

    wait_until_ist "$PROBE_POLL_START_IST"
    local verdict="" waited=0 timeout=$((PROBE_TIMEOUT_MIN * 60))
    while ((waited < timeout)); do
      if manual_stop_active "$MANUAL_STOP_MARKER" "$TODAY"; then
        log "manual stop during probe wait — standing down"
        monitor_until_eod
        eod_summary "$day"
        exit 0
      fi
      verdict=$(fetch_probe_verdict || true)
      [ -n "$verdict" ] && break
      sleep 30
      waited=$((waited + 30))
    done

    local action
    action=$(next_action_for_verdict "${verdict:-}")
    log "probe verdict: '${verdict:-<none>}' → action: $action"
    case "$action" in
    scale_max)
      tg "✅ PROBE VERDICT: both connections captured — the 1000 cap is PER-CONNECTION. Switching to the FULL 100K ladder now."
      stop_app
      apply_overlay max
      start_app "max-scale ladder" || exit 1
      ;;
    record_continue)
      tg "🔔 PROBE VERDICT: per-ACCOUNT limit — only one connection streams. That IS the experiment's answer; recorded in the audit table. Continuing a NORMAL session (no laddering)."
      stop_app
      apply_overlay none
      start_app "normal session after per-account verdict" || true
      ;;
    continue_normal)
      tg "⚠️ Probe inconclusive (base connection captured nothing) — infra/auth problem, not a limit answer. Continuing a NORMAL session; check the Groww token / feed page."
      stop_app
      apply_overlay none
      start_app "normal session after inconclusive probe" || true
      ;;
    none)
      tg "⚠️ Probe verdict TIMED OUT after ${PROBE_TIMEOUT_MIN}min — treating as inconclusive; continuing a NORMAL session. Check the app log + feed page."
      stop_app
      apply_overlay none
      start_app "normal session after probe timeout" || true
      ;;
    esac
  else
    apply_overlay none # normal local day: branch overlay (feeds ON) applies
    start_app "normal local day" || exit 1
    tg "🚀 Normal local day: app booted, both-feed capture live (dry_run — no orders)."
  fi

  monitor_until_eod

  # ── 15:35 IST: graceful end of day (skip app-stop if manual stop won). ──
  if ! manual_stop_active "$MANUAL_STOP_MARKER" "$TODAY"; then
    stop_app
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
  fi
  stop_caffeinate
  eod_summary "$day"
  log "===== LOCAL AUTOPILOT run complete ====="
}

case "${1:-run}" in
run) cmd_run ;;
start) cmd_start ;;
stop) cmd_stop ;;
dev) cmd_dev ;;
status) cmd_status ;;
*)
  echo "usage: $0 [run|start|stop|dev|status]" >&2
  exit 2
  ;;
esac
