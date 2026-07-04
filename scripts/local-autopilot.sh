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
LOG_DIR="${LOG_DIR:-data/logs/autopilot}"              # ALL autopilot/dev-run log lines — ONE rolling file
LOG_KEEP="${LOG_KEEP:-7}"                              # rotated day-files kept; older deleted
MANUAL_STOP_MARKER="${MANUAL_STOP_MARKER:-data/local-manual-stop.marker}"
SCALE_WINDOW_START="${SCALE_WINDOW_START:-2026-07-06}"
SCALE_WINDOW_END="${SCALE_WINDOW_END:-2026-07-08}"
MIN_DISK_FREE_GB="${MIN_DISK_FREE_GB:-50}"
PROBE_POLL_START_IST="${PROBE_POLL_START_IST:-09:45}"
PROBE_TIMEOUT_MIN="${PROBE_TIMEOUT_MIN:-20}"
EOD_STOP_IST="${EOD_STOP_IST:-15:35}"
CAFFEINATE_UNTIL_IST="${CAFFEINATE_UNTIL_IST:-15:45}"
MONITOR_INTERVAL_SECS="${MONITOR_INTERVAL_SECS:-60}"
QUESTDB_HTTP="${QUESTDB_HTTP:-http://127.0.0.1:9000}"
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

# log_rotate_needed <last_stamp_date> <today> → yes|no.
# Day-change rotation for the ONE common autopilot.log (operator 2026-07-04:
# "one common log folder and one common log file with rolling appender").
log_rotate_needed() {
  if [ -n "${1:-}" ] && [ "${1:-}" != "$2" ]; then echo yes; else echo no; fi
}

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

# ── End of pure functions. Library mode stops here. ────────────────────────
if [[ "${AUTOPILOT_LIB:-0}" == "1" ]]; then
  return 0 2>/dev/null || exit 0
fi

set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p "$AUTOPILOT_DIR" "$LOG_DIR"

TODAY="$(ist_date)"
# ONE COMMON LOG FILE (operator 2026-07-04: "just have one common log folder
# and one common log file with rolling appender"). Every autopilot line, the
# cargo build output, AND the app's stdout/stderr land in this single file.
# Rolling: at day change the file rotates to autopilot.log.<date>; the newest
# LOG_KEEP rotated files are kept, older deleted. State files (pids, markers,
# lock, stamp) stay in data/local-autopilot/ — state, not logs. The app's own
# canonical sinks (data/logs/app.*.log, errors.log, errors.jsonl.*) are
# RULE-LOCKED (observability-architecture.md) and untouched.
LOG="$LOG_DIR/autopilot.log"

rotate_autopilot_log() {
  local stamp="$AUTOPILOT_DIR/.log-date" last=""
  [ -f "$stamp" ] && last="$(cat "$stamp" 2>/dev/null || true)"
  if [ "$(log_rotate_needed "$last" "$TODAY")" = "yes" ] && [ -f "$LOG" ]; then
    mv "$LOG" "$LOG_DIR/autopilot.log.$last" 2>/dev/null || true
    local rotated=("$LOG_DIR"/autopilot.log.*)
    if [ -e "${rotated[0]}" ]; then
      while IFS= read -r doomed; do
        if [ -n "$doomed" ]; then rm -f "$doomed"; fi
      done < <(rotated_logs_to_delete "$LOG_KEEP" "${rotated[@]}")
    fi
  fi
  echo "$TODAY" >"$stamp"
}
rotate_autopilot_log

# log(): append to the ONE file; echo to the terminal only when interactive.
# (The launchd plist points its own stdout/stderr at the SAME file — a tee
# to stdout there would double-write every line.)
log() {
  local line="[$(ist_hms) IST] $*"
  echo "$line" >>"$LOG"
  if [ -t 1 ]; then echo "$line"; fi
}

tg() { # best-effort Telegram; the log always carries the message
  log "TELEGRAM: $*"
  bash scripts/notify-telegram.sh "💻 LOCAL autopilot — $*" >>"$LOG" 2>&1 || true
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
  if [ "$(uname -s)" = "Darwin" ]; then open -a Docker >>"$LOG" 2>&1 || true; fi
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

ensure_questdb() {
  docker compose -f deploy/docker/docker-compose.yml \
    -f deploy/docker/docker-compose.local.yml up -d tv-questdb >>"$LOG" 2>&1
  local i
  for i in $(seq 1 24); do
    if curl -sf -G "$QUESTDB_HTTP/exec" --data-urlencode "query=select 1" >/dev/null 2>&1; then
      log "QuestDB HTTP ready"
      return 0
    fi
    sleep 5
  done
  tg "🆘 QuestDB did not answer on $QUESTDB_HTTP within 120s. If another program holds port 9000, quit it (lsof -i :9000) and double-click Start TickVault."
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

start_app() { # $1 = label for the log
  if app_alive; then
    log "app already running (pid $(cat "$APP_PIDFILE")) — not double-starting"
    return 0
  fi
  log "building release binary (first build of the day can take minutes)..."
  if ! cargo build --release >>"$LOG" 2>&1; then
    tg "🆘 Build FAILED — the app cannot start. Last lines: $(tail -5 "$LOG" | tr '\n' ' | ')"
    return 1
  fi
  nohup ./target/release/tickvault >>"$LOG" 2>&1 &
  echo $! >"$APP_PIDFILE"
  log "app launched ($1) — pid $(cat "$APP_PIDFILE"), log $LOG"
  sleep 60
  if ! app_alive; then
    tg "🆘 App DIED within 60s of launch ($1). Log tail: $(tail -8 "$LOG" | tr '\n' ' | ')"
    return 1
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
    bash scripts/groww-scale-test.sh clean >>"$LOG" 2>&1
  else
    DRY_RUN=1 bash scripts/groww-scale-test.sh "$1" >>"$LOG" 2>&1
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
  bash scripts/groww-scale-test.sh clean >>"$LOG" 2>&1 || true
  stop_caffeinate
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
  # Tail the APP's own canonical log (data/logs/app.YYYY-MM-DD.log — the
  # human sink, rule-locked in observability-architecture.md) rather than
  # duplicating it; fall back to the common autopilot.log if the app has not
  # created its file yet.
  local app_log
  app_log=$(ls -t data/logs/app.*.log 2>/dev/null | head -1 || true)
  if [ -z "$app_log" ]; then app_log="$LOG"; fi
  echo "=== tickvault is RUNNING — tailing $app_log ==="
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
  echo "logs   : $LOG (single rolling file; app log = data/logs/app.<date>.log)"
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
    if ! git checkout local-runtime >>"$LOG" 2>&1; then
      tg "⚠️ Could not switch to the local-runtime branch (uncommitted changes?). Running the EXISTING checkout '$branch' — code may be stale."
      return 0
    fi
  fi
  if git fetch origin >>"$LOG" 2>&1; then
    if ! git merge --ff-only origin/local-runtime >>"$LOG" 2>&1; then
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
  errors=$(grep -c "ERROR" "$LOG" 2>/dev/null || true)
  if [ -f "$summary" ]; then
    digest=$'\nStage table (outcome | conns | proof | capturing | bytes | cpu | mem):\n'
    digest+=$(awk -F'\t' 'NR>1 {printf "%s | %s | %s | %s | %s | %s%% | %s%%\n", $2, $3, $6, $7, $8, $9, $10}' "$summary" | tail -12)
  fi
  tg "🔔 End of day ($mode day). App stopped 15:35 IST, overlay cleaned. ERROR lines in app log: ${errors:-0}.${digest}"
}

monitor_until_eod() {
  local relaunches=0 decision alive manual
  while :; do
    if (($(secs_until "$EOD_STOP_IST" "$(ist_hms)") == 0)); then break; fi
    sleep "$MONITOR_INTERVAL_SECS"
    alive=0
    app_alive && alive=1
    manual=0
    manual_stop_active "$MANUAL_STOP_MARKER" "$TODAY" && manual=1
    decision=$(monitor_decision "$alive" "$manual" "$relaunches")
    case "$decision" in
    idle) : ;;
    manual_standdown)
      log "manual stop marker present — autopilot stands down (no relaunch, no fight)"
      ;;
    relaunch)
      relaunches=$((relaunches + 1))
      tg "⚠️ App died mid-session — relaunching once (attempt $relaunches). Log tail: $(tail -5 "$LOG" | tr '\n' ' | ')"
      sleep 30
      start_app "auto-relaunch" || true
      ;;
    alert)
      tg "🆘 App died AGAIN after the one relaunch — NOT retrying (avoid crash-looping the feed). Log tail: $(tail -8 "$LOG" | tr '\n' ' | '). Double-click Start TickVault after investigating."
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
    bash scripts/groww-scale-test.sh clean >>"$LOG" 2>&1 || true
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
