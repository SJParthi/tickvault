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
# B2: the 08:55 robot's boot chain (git/Docker/disk/database) gets a bounded
# retry on transient failure — every BOOT_RETRY_INTERVAL_SECS until
# BOOT_RETRY_DEADLINE_IST, one Telegram on the first failure. Previously any
# single blip at 08:55 killed the whole trading day's capture.
BOOT_RETRY_DEADLINE_IST="${BOOT_RETRY_DEADLINE_IST:-09:10}"
BOOT_RETRY_INTERVAL_SECS="${BOOT_RETRY_INTERVAL_SECS:-300}"
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
# /tickvault/<env>/telegram/<key>). Pure; empty env defaults to prod —
# the SAME default the app's resolve_environment uses (FIX 8.1; the earlier
# dev default pointed at a parameter that does not exist in SSM).
# FIX 5.2: the launcher's Telegram previously read /dlt/<env>/telegram/* —
# another project's namespace — so every launcher Telegram silently failed.
tg_ssm_path() { echo "/tickvault/${1:-prod}/telegram/$2"; }

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
# Desktop ≥ 4.30) or "memoryMiB" (legacy settings.json). FIX 14: a file
# that parses as a JSON dict but carries NEITHER key (a fresh
# settings-store.json before the user ever changed memory) prints the
# modern default "MemoryMiB" and returns rc 10 — the caller INSERTS the
# key instead of giving up. Distinct failure codes so the caller can log
# WHY: rc 2 = file absent, rc 3 = not valid JSON / not a dict, any other
# nonzero = python3 itself failed to run. JSON round-trip via python3 —
# never sed on a settings file.
docker_settings_mem_key() {
  [ -f "${1:-}" ] || return 2
  python3 - "$1" <<'PY'
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
except Exception:
    sys.exit(3)
if not isinstance(d, dict):
    sys.exit(3)
for k in ("MemoryMiB", "memoryMiB"):
    if k in d:
        print(k)
        sys.exit(0)
# Neither key present yet — modern store default; caller inserts (FIX 14).
print("MemoryMiB")
sys.exit(10)
PY
}

# docker_settings_set_mem <settings-json-file> <key> <mib> — set the memory
# key to <mib> via a python3 JSON round-trip (temp file + atomic replace;
# every other setting preserved). FIX 14: the key is INSERTED when absent
# (a fresh settings-store.json carries no memory key until the user edits
# it). rc 1 on missing file / bad JSON / non-numeric mib — the FILE IS
# UNTOUCHED on every failure path.
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
if not isinstance(d, dict):
    sys.exit(1)
d[key] = mib  # FIX 14: inserts when absent, updates when present
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

# ── FIX 10 (PUSH A): probe correctness + launcher safety — pure helpers ────

# tsv_sentinel <raw wc -l output> → a clean integer line count (macOS wc
# pads with spaces; empty/garbage → 0). A1: the probe evaluates ONLY rows
# appended AFTER its own start — earlier scale runs the same day share the
# append-per-day TSV and previously produced a FALSE verdict.
tsv_sentinel() {
  local v="${1:-}"
  v="${v//[[:space:]]/}"
  case "$v" in '' | *[!0-9]*) echo 0 ;; *) echo "$v" ;; esac
}

# has_scale_overlay <config file> → rc 0 when the file carries the scale
# harness managed block or in-place key overrides. A3: a window-close /
# SIGHUP / reboot mid-probe leaks the max-smoke overlay — the next NORMAL
# run must detect + remove it instead of silently booting Dhan-OFF.
has_scale_overlay() {
  [ -f "${1:-}" ] || return 1
  grep -qE '^# >>> groww-scale-test |scale-override\(was: ' "$1"
}

# launcher_lock_stale <pid> → rc 0 (stale — safe to steal) when the pid is
# empty/garbage or no longer alive. A4 mutual-exclusion support.
launcher_lock_stale() {
  case "${1:-}" in '' | *[!0-9]*) return 0 ;; esac
  ! kill -0 "$1" 2>/dev/null
}

# manual_stop_during_probe <marker_epoch> <probe_start_epoch> [grace=120]
# → rc 0 when the manual-stop marker was written NOTICEABLY AFTER probe
# start — i.e. the OPERATOR pressed Stop during the probe (must stand; no
# auto live-start). The probe's own internal stop writes the marker within
# seconds of probe start, inside the grace window. Garbage → rc 1
# (fail-safe: treat as not-an-operator-stop). Honest limitation: an
# operator Stop within the first ~2 minutes is indistinguishable from the
# probe's own stop and is cleared.
manual_stop_during_probe() {
  local m="${1:-}" s="${2:-}" g="${3:-120}"
  case "$m" in '' | *[!0-9]*) return 1 ;; esac
  case "$s" in '' | *[!0-9]*) return 1 ;; esac
  case "$g" in '' | *[!0-9]*) g=120 ;; esac
  ((m > s + g))
}

# ── FIX 10 (PUSH B): Monday-morning robustness — pure helpers ───────────────

# boot_retry_should_retry <secs_until_deadline> → rc 0 when a failed boot
# chain should be retried (deadline not yet reached). B2: a transient
# 08:55 failure (network blip, Docker still waking, database slow) must
# not kill the whole trading day — retry every 5 min until 09:10 IST.
boot_retry_should_retry() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  (("$1" > 0))
}

# pid_cmdline_is_tickvault <ps command line> → rc 0 when the process is our
# release binary. B5: a pidfile pid can be RECYCLED by macOS to a totally
# different program after a crash — adopt/kill must never touch a foreign
# process.
pid_cmdline_is_tickvault() {
  case "${1:-}" in *target/release/tickvault*) return 0 ;; *) return 1 ;; esac
}

# etime_to_secs <ps -o etime= value> → elapsed seconds on stdout, or empty
# when unparseable (caller then SKIPS the start-time plausibility check —
# fail-open on the time leg; the cmdline check above remains mandatory).
# Formats: mm:ss, hh:mm:ss, dd-hh:mm:ss all handled.
etime_to_secs() {
  local e="${1:-}" d=0 h=0 m=0 s=0 rest v
  if [ -z "$e" ]; then
    echo ""
    return 0
  fi
  case "$e" in
  *-*)
    d="${e%%-*}"
    rest="${e#*-}"
    ;;
  *) rest="$e" ;;
  esac
  local IFS=':'
  # shellcheck disable=SC2086
  set -- $rest
  case $# in
  3)
    h=$1 m=$2 s=$3
    ;;
  2)
    m=$1 s=$2
    ;;
  1) s=$1 ;;
  *)
    echo ""
    return 0
    ;;
  esac
  for v in "$d" "$h" "$m" "$s"; do
    case "$v" in '' | *[!0-9]*)
      echo ""
      return 0
      ;;
    esac
  done
  echo $((10#$d * 86400 + 10#$h * 3600 + 10#$m * 60 + 10#$s))
}

# pid_start_plausible <proc_start_epoch> <pidfile_mtime_epoch> → rc 0 when
# the process could genuinely be the one the pidfile points at: it started
# AT/BEFORE the pidfile write (+60s clock slack). A recycled pid always
# starts AFTER the pidfile was written. Garbage → rc 1 (fail-safe: treat
# as not-ours; the launcher then treats it as no-app).
pid_start_plausible() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  case "${2:-}" in '' | *[!0-9]*) return 1 ;; esac
  (("$1" <= "$2" + 60))
}

# questdb_owner_verdict <docker inspect state string> → ok|foreign|not_running.
# B6: "port 9000 answers" is NOT proof OUR database is up — any local
# process could hold the port. Empty state = the tv-questdb container does
# not even exist while something answers → foreign responder.
questdb_owner_verdict() {
  local s="${1:-}"
  if [ -z "$s" ]; then
    echo foreign
    return 0
  fi
  case "$s" in
  running*) echo ok ;;
  *) echo not_running ;;
  esac
}

# ── FIX 9: one-shot probe-then-run marker (two-buttons-forever) ─────────────

# probe_once_decision <marker_date> <today> <stamp_exists 0|1> → run|skip.
# Pure. "run" ONLY when the COMMITTED marker (deploy/local/probe-once.date)
# is a well-formed YYYY-MM-DD equal to TODAY (IST) and no local completion
# stamp exists — so the marker is inert on every other day and after the one
# attempt. The operator's interface stays EXACTLY two buttons: a dated
# marker committed by the coordinator makes the NEXT "Run tickvault" click
# run the one-time lab probe first, then continue into the normal start.
probe_once_decision() {
  [ "${3:-1}" = "0" ] || {
    echo skip
    return 0
  }
  case "${1:-}" in
  [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
  *)
    echo skip
    return 0
    ;;
  esac
  if [ "$1" = "${2:-}" ]; then echo run; else echo skip; fi
}

# ── FIX 11: attempt-numbered probe-once marker ──────────────────────────────
# The marker line may be `YYYY-MM-DD` (= attempt 1, backward compatible) or
# `YYYY-MM-DD N`. The completion stamp is per-attempt
# (probe-once.done.<date>.<N>), so committing the marker as "<date> 2"
# re-arms EXACTLY ONE more probe the same day after a fix — still one-shot
# per attempt, interface still two buttons.

# probe_marker_date <marker line> → the date (1st whitespace field). Pure.
probe_marker_date() { printf '%s\n' "${1:-}" | awk '{print $1}'; }

# probe_marker_attempt <marker line> → the attempt number (2nd field);
# missing / garbage / 0 → 1 (a bare-date marker is attempt 1). Pure.
probe_marker_attempt() {
  local a
  a=$(printf '%s\n' "${1:-}" | awk '{print $2}')
  case "$a" in '' | *[!0-9]* | 0) echo 1 ;; *) echo "$a" ;; esac
}

# ── FIX 8: launcher Telegram env + stale-adopt / recreated-DB guards ───────

# tg_env_resolve <tv_environment> <environment> → the SSM environment the
# APP itself resolves (secret_manager.rs resolve_environment precedence:
# TV_ENVIRONMENT → ENVIRONMENT → "prod"). FIX 8.1: the launcher previously
# defaulted to "dev" while the app defaults to "prod" — so the launcher read
# a nonexistent /tickvault/dev/telegram/* param while the app's prod one
# worked from the same Mac. Pure.
tg_env_resolve() {
  if [ -n "${1:-}" ]; then
    echo "$1"
  elif [ -n "${2:-}" ]; then
    echo "$2"
  else
    echo prod
  fi
}

# sha_compare <app_sha> <repo_sha> → match|mismatch|unknown. Pure. FIX 8.2:
# an adopted app may be an OLD build; the B9 provenance /health git_sha vs
# `git rev-parse HEAD` decides. Empty or the literal "unknown" (a build
# without git) on either side → "unknown" (conservative: adopt + honest log).
sha_compare() {
  local a="${1:-}" r="${2:-}"
  if [ -z "$a" ] || [ -z "$r" ] || [ "$a" = "unknown" ] || [ "$r" = "unknown" ]; then
    echo unknown
    return 0
  fi
  if [ "$a" = "$r" ]; then echo match; else echo mismatch; fi
}

# db_probe_verdict <questdb /exec response body> → ok|missing|unknown. Pure.
# FIX 8.3: after a QuestDB container RECREATE the app-owned tables are gone
# until the app's boot DDL re-runs — probing a canonical app-owned table
# (ws_event_audit) tells us whether the adopted app's tables still exist.
db_probe_verdict() {
  case "${1:-}" in
  *'table does not exist'*) echo missing ;;
  *'"dataset"'*) echo ok ;;
  *) echo unknown ;;
  esac
}

# in_market_hours_ist <secs_of_day> <dow 1..7> → rc 0 during the NSE session
# window 09:00–15:30 IST on a weekday. Pure; garbage input → rc 1 (treated
# as off-hours: restarts stay allowed — the conservative default for a
# maintenance action is "not mid-market").
in_market_hours_ist() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  case "${2:-}" in [1-5]) ;; *) return 1 ;; esac
  (($1 >= 32400 && $1 < 55800))
}

# adopt_health_decision <sha_state match|mismatch|unknown>
#                       <db_state ok|missing|unknown> <market_open 0|1>
# → adopt | warn_stale | warn_db | restart_stale | restart_db. Pure.
# Rules: a missing database table set outranks a stale build; during market
# hours a healthy streaming app is NEVER auto-restarted (warn/page instead);
# unknown states adopt conservatively (runtime logs honestly).
adopt_health_decision() {
  local sha="${1:-unknown}" db="${2:-unknown}" mkt="${3:-0}"
  if [ "$db" = "missing" ]; then
    if [ "$mkt" = "1" ]; then echo warn_db; else echo restart_db; fi
    return 0
  fi
  if [ "$sha" = "mismatch" ]; then
    if [ "$mkt" = "1" ]; then echo warn_stale; else echo restart_stale; fi
    return 0
  fi
  echo adopt
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
  # A7: a full disk must degrade to console output, never kill the monitor
  # loop under set -e — every write here is guarded.
  local line="[$(ist_hms) IST] [launcher] $*"
  echo "$line" >>"$(current_log)" 2>/dev/null || echo "$line" >&2 || true
  if [ -t 1 ]; then echo "$line" || true; fi
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
  # FIX 8.1: resolve the SSM environment the way the APP does
  # (TV_ENVIRONMENT → ENVIRONMENT → "prod"). The launcher's previous
  # dev-default read /tickvault/dev/telegram/* — a parameter that does not
  # exist — while the app's Telegram worked from the same Mac via the prod
  # default. Try the resolved env first, then fall back to prod, and log
  # WHICH path worked. Region stays ap-south-1 (same as bootstrap.sh) so a
  # different Mac-default AWS region cannot break the read.
  local env candidates cand tok chat
  env="$(tg_env_resolve "${TV_ENVIRONMENT:-}" "${ENVIRONMENT:-}")"
  candidates="$env"
  [ "$env" != "prod" ] && candidates="$env prod"
  if command -v aws >/dev/null 2>&1; then
    for cand in $candidates; do
      tok=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
        --name "$(tg_ssm_path "$cand" bot-token)" \
        --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
      chat=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
        --name "$(tg_ssm_path "$cand" chat-id)" \
        --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
      if [ -n "$tok" ] && [ "$tok" != "None" ] && [ -n "$chat" ] && [ "$chat" != "None" ]; then
        export TV_TELEGRAM_BOT_TOKEN="$tok" TV_TELEGRAM_CHAT_ID="$chat"
        log "Telegram credentials loaded from $(tg_ssm_path "$cand" bot-token) (region ap-south-1)"
        return 0
      fi
      log "Telegram parameter $(tg_ssm_path "$cand" bot-token) unreadable — trying next candidate"
    done
  fi
  TG_DISABLED=1
  log "Telegram unavailable (tried env path(s): ${candidates// /, } under /tickvault/<env>/telegram/ in ap-south-1 — no internet, no AWS credentials, or missing parameter). Messages will appear in this log only for the rest of this run."
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
    export TV_QUESTDB_PG_USER="$u" TV_QUESTDB_PG_PASSWORD="$p" # secret-scan-ignore: values read from AWS SSM above, never hardcoded
  else
    log "QuestDB credentials unreadable from SSM (/tickvault/${env}/questdb/*) — the database container can still START if it already exists, but compose cannot (re)create it until the Mac is online"
  fi
}

# ── Service primitives (shared by autopilot + manual start/stop) ───────────

# file_mtime_epoch <file> → mtime as epoch seconds (empty when unreadable).
# macOS (stat -f) first, Linux/CI (stat -c) fallback.
file_mtime_epoch() {
  stat -f %m "${1:-}" 2>/dev/null || stat -c %Y "${1:-}" 2>/dev/null || echo ""
}

# B5: PID-reuse guard. A pidfile pid that a crashed launcher left behind
# can be RECYCLED by the OS to a completely different program — blind
# kill -0 then adopted/killed a FOREIGN process. pid_is_our_app verifies
# (a) the pid is alive, (b) its command line IS the tickvault release
# binary, and (c) when a pidfile mtime is supplied, the process started
# AT/BEFORE the pidfile write (a recycled pid always starts after).
# Foreign pid → rc 1 (treat as no-app) + a once-per-episode Telegram via a
# marker file (a variable latch would be lost in command-substitution
# subshells). NEVER kills anything itself.
FOREIGN_PID_MARK="$AUTOPILOT_DIR/foreign-pid-warned"
pid_is_our_app() { # $1 = pid, $2 = pidfile mtime epoch (optional)
  local pid="${1:-}" mtime="${2:-}" cmd et secs start
  case "$pid" in '' | *[!0-9]*) return 1 ;; esac
  kill -0 "$pid" 2>/dev/null || return 1
  cmd=$(ps -o command= -p "$pid" 2>/dev/null || true)
  if ! pid_cmdline_is_tickvault "$cmd"; then
    log "pid $pid from the pidfile is NOT the tickvault app (it is: ${cmd:-unknown}) — treating as no-app; never touching a foreign program"
    if [ ! -f "$FOREIGN_PID_MARK" ]; then
      touch "$FOREIGN_PID_MARK" 2>/dev/null || true
      tg "⚠️ The app's saved process number now belongs to a DIFFERENT program (the app is not actually running). Treating it as stopped — the other program is left alone."
    fi
    return 1
  fi
  if [ -n "$mtime" ]; then
    et=$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ' || true)
    secs=$(etime_to_secs "$et")
    if [ -n "$secs" ]; then
      start=$(($(date +%s) - secs))
      if ! pid_start_plausible "$start" "$mtime"; then
        log "pid $pid started AFTER the pidfile was written (recycled pid) — treating as no-app"
        return 1
      fi
    fi
  fi
  return 0
}

app_alive() {
  [ -f "$APP_PIDFILE" ] || return 1
  pid_is_our_app "$(cat "$APP_PIDFILE" 2>/dev/null || true)" "$(file_mtime_epoch "$APP_PIDFILE")"
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
  # B6: a compose failure must not be swallowed — log it loudly and keep
  # probing (the container may already be running from an earlier start),
  # but the timeout message will name the compose failure as the lead cause.
  local compose_failed=0
  if ! docker compose -f deploy/docker/docker-compose.yml \
    -f deploy/docker/docker-compose.local.yml up -d tv-questdb >>"$(current_log)" 2>&1; then
    compose_failed=1
    log "docker compose up for the database FAILED (details above in this log) — still probing in case the container is already running"
  fi
  # Self-heal (2026-07-04): a degraded container (unhealthy/exited/dead/
  # paused — e.g. after a hard Mac sleep or an out-of-memory kill) gets ONE
  # automatic `docker restart` if the database has not answered after ~30s.
  # Zero operator action — Stop→Start (or just Start) recovers it.
  local i restarted=0 state verdict
  for i in $(seq 1 24); do
    if curl -sf -G "$QUESTDB_HTTP/exec" --data-urlencode "query=select 1" >/dev/null 2>&1; then
      # B6: verify the responder IS our tv-questdb container — any local
      # process could hold port 9000 (a foreign QuestDB, a dev server).
      # Writing the day's capture into a stranger's database is silent loss.
      verdict="$(questdb_owner_verdict "$(questdb_container_state)")"
      if [ "$verdict" != "ok" ]; then
        log "port answers but the tv-questdb container is '$verdict' — a foreign program is on the database port"
        tg "🆘 Something else is answering on the database port and it is NOT our database container. Quit that other program (check Docker Desktop / anything using port 9000), then double-click Start TickVault."
        return 1
      fi
      log "QuestDB HTTP ready (tv-questdb container confirmed running)"
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
  if ((compose_failed)); then
    tg "🆘 Docker could not bring the database container up (the compose command itself failed) and the database never answered. Open Docker Desktop, make sure it is running, then double-click Start TickVault."
  else
    tg "🆘 The database did not start within 2 minutes. Double-click Stop TickVault, wait 10 seconds, then double-click Start TickVault. If it still fails, restart the Mac and double-click Start TickVault again."
  fi
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

# ── FIX 10 A4: launcher mutual exclusion ────────────────────────────────────
# Two launchers at once (double Run click, 08:54 manual vs 08:55 robot)
# previously raced the pidfile and could double-launch the app. A mkdir
# lock (atomic on every filesystem) around the start/stop critical
# sections serializes them; a dead holder's lock is stolen (pid check).
LAUNCHER_LOCK_DIR="${LAUNCHER_LOCK_DIR:-data/launcher.lock}"
launcher_lock_acquire() {
  if mkdir "$LAUNCHER_LOCK_DIR" 2>/dev/null; then
    echo $$ >"$LAUNCHER_LOCK_DIR/pid" 2>/dev/null || true
    return 0
  fi
  local other
  other=$(cat "$LAUNCHER_LOCK_DIR/pid" 2>/dev/null || true)
  if launcher_lock_stale "$other"; then
    log "stale launcher lock (pid ${other:-?} is gone) — taking over"
    rm -rf "$LAUNCHER_LOCK_DIR" 2>/dev/null || true
    if mkdir "$LAUNCHER_LOCK_DIR" 2>/dev/null; then
      echo $$ >"$LAUNCHER_LOCK_DIR/pid" 2>/dev/null || true
      return 0
    fi
  fi
  return 1
}
launcher_lock_release() {
  # Only the owner releases (best-effort).
  if [ "$(cat "$LAUNCHER_LOCK_DIR/pid" 2>/dev/null || true)" = "$$" ]; then
    rm -rf "$LAUNCHER_LOCK_DIR" 2>/dev/null || true
  fi
}
launcher_lock_acquire_wait() { # $1 = max seconds to wait
  local waited=0
  while ! launcher_lock_acquire; do
    if ((waited == 0)); then
      log "another launcher is mid-flight (lock: $LAUNCHER_LOCK_DIR, pid $(cat "$LAUNCHER_LOCK_DIR/pid" 2>/dev/null || echo '?')) — waiting up to ${1}s"
    fi
    sleep 2
    waited=$((waited + 2))
    if ((waited >= $1)); then return 1; fi
  done
  return 0
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

# app_git_sha — the running app's build provenance from GET /health (B9
# git_sha field). Empty on any failure (app down, old build without the
# field). Runtime (network); the compare itself is the pure sha_compare.
app_git_sha() {
  curl -sf -m 3 "http://127.0.0.1:${APP_API_PORT}/health" 2>/dev/null |
    python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("git_sha", ""))
except Exception:
    pass' 2>/dev/null || true
}

# db_app_tables_probe → ok|missing|unknown. Probes a canonical APP-owNED
# table (ws_event_audit — created by the app boot DDL, not by schema-init)
# via QuestDB /exec. NOTE: no curl -f — QuestDB answers a missing table
# with HTTP 400 + a JSON error body, which is exactly the signal we parse.
db_app_tables_probe() {
  local body
  body=$(curl -s -m 3 --get "${QDB_HTTP_URL:-http://127.0.0.1:9000}/exec" \
    --data-urlencode "query=select 1 from ws_event_audit limit 1" 2>/dev/null || true)
  db_probe_verdict "$body"
}

# adopt_gate — FIX 8.2 + 8.3: decide whether a healthy running app may be
# ADOPTED as today's app. Checks (a) build freshness: /health git_sha vs
# repo HEAD (a self-updated repo must not keep Monday's robot on Friday's
# binary), and (b) database integrity: the app-owned tables still exist
# (a QuestDB container RECREATE after the app booted wipes them — the
# app's boot DDL must re-run, i.e. the app must restart). stdout: the
# adopt_health_decision verdict. log() inside is safe — its tty echo is
# suppressed under command substitution.
adopt_gate() {
  local app_sha repo_sha sha_state db_state mkt=0
  app_sha="$(app_git_sha)"
  repo_sha="$(git rev-parse HEAD 2>/dev/null || true)"
  sha_state="$(sha_compare "$app_sha" "$repo_sha")"
  db_state="$(db_app_tables_probe)"
  # ist_weekday (NOT host-local `date +%u`): the time-of-day is IST, so the
  # day-of-week must be too — on a non-IST Mac (e.g. US TZ) 10:00 IST Monday
  # is Sunday evening locally, which would misread mid-market as off-hours
  # and allow an auto-restart of a healthy streaming app during the session.
  if in_market_hours_ist "$(hhmm_to_secs "$(ist_hms)")" "$(ist_weekday)"; then mkt=1; fi
  if [ "$sha_state" = "unknown" ]; then
    log "adopt check: build version unverifiable (health/sha unavailable) — adopting conservatively"
  fi
  if [ "$db_state" = "unknown" ]; then
    log "adopt check: database table probe inconclusive — adopting conservatively"
  fi
  adopt_health_decision "$sha_state" "$db_state" "$mkt"
}

# handle_adopt_verdict <verdict> <pid> — apply an adopt_gate verdict to a
# running app. rc 0 = adopted (caller returns); rc 1 = the app was stopped
# and the caller must fall through to a fresh launch.
handle_adopt_verdict() {
  local verdict="$1" pid="$2"
  case "$verdict" in
  adopt)
    echo "$pid" >"$APP_PIDFILE"
    log "adopted running app pid $pid (build + database check out — no relaunch needed)"
    write_run_stamp
    return 0
    ;;
  warn_stale)
    echo "$pid" >"$APP_PIDFILE"
    log "running app is an OLDER build than the repo — market hours, so NOT restarting a healthy streaming app; it will pick up the new build at the next off-hours start"
    tg "ℹ️ The running app is an older version than the latest update. It keeps running through market hours; the new version starts automatically at the next restart."
    write_run_stamp
    return 0
    ;;
  warn_db)
    echo "$pid" >"$APP_PIDFILE"
    log "database looks RECREATED after the app booted (app tables missing) — market hours, so NOT auto-restarting; operator paged"
    tg "🆘 The database was recreated AFTER the app started, so the app's tables are missing and new data may not be stored. Double-click Stop TickVault then Start TickVault as soon as convenient."
    write_run_stamp
    return 0
    ;;
  restart_stale)
    log "running app is an older build (sha $(app_git_sha | cut -c1-7) vs repo $(git rev-parse --short HEAD 2>/dev/null)) — restarting on the new build"
    ;;
  restart_db)
    log "database was recreated after the app booted — restarting app so it can rebuild its tables"
    ;;
  esac
  # restart_* verdicts: stop the old app politely, then force.
  kill "$pid" 2>/dev/null || true
  local w
  for w in $(seq 1 10); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -9 "$pid" 2>/dev/null || true
    sleep 1
  fi
  rm -f "$APP_PIDFILE"
  return 1
}

# A4: the launch critical section runs under the launcher lock — a second
# entrant (double click / manual-vs-robot overlap) waits briefly, then
# SKIPS (the other launcher owns the start; adopt-on-next-click covers it).
start_app() { # $1 = label for the log
  if ! launcher_lock_acquire_wait 60; then
    log "another launcher still owns the start after 60s — skipping this launch to avoid a double-start (the other launcher is handling it)"
    return 0
  fi
  local _rc=0
  start_app_inner "$@" || _rc=$?
  launcher_lock_release
  return $_rc
}

start_app_inner() { # $1 = label for the log
  if app_alive; then
    # FIX 8: a pidfile-alive app gets the SAME freshness + database gate as
    # a discovered one — otherwise Monday's robot keeps Friday's binary (or
    # an app whose tables were wiped by a database recreate) alive forever.
    local kept_pid
    kept_pid=$(cat "$APP_PIDFILE")
    if handle_adopt_verdict "$(adopt_gate)" "$kept_pid"; then
      log "app already running (pid $kept_pid) — not double-starting"
      return 0
    fi
    log "previous app (pid $kept_pid) stopped by the adopt gate — launching fresh"
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
    # FIX 8: a healthy discovered app must ALSO pass the build-freshness +
    # database-integrity gate before being adopted as today's app.
    if handle_adopt_verdict "$(adopt_gate)" "$existing"; then
      return 0
    fi
    log "discovered app (pid $existing) stopped by the adopt gate — launching fresh"
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
  rm -f "$FOREIGN_PID_MARK" 2>/dev/null || true # B5: re-arm the foreign-pid alert
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
  # then the legacy file). FIX 14: each failure mode gets its own
  # plain-English log line, and a valid file with no memory key gets the
  # key INSERTED instead of being rejected.
  local gc="$HOME/Library/Group Containers/group.com.docker"
  local file="" key="" cand rc unusable=0
  for cand in "$gc/settings-store.json" "$gc/settings.json"; do
    # errexit-safe capture: the || arm keeps a nonzero rc from aborting.
    key=$(docker_settings_mem_key "$cand") && rc=0 || rc=$?
    case "$rc" in
    0)
      file="$cand"
      break
      ;;
    10)
      file="$cand"
      log "Docker settings '$(basename "$cand")' has no memory size entry yet — inserting a new ${key} entry"
      break
      ;;
    2)
      : # this candidate file does not exist — try the next one
      ;;
    3)
      unusable=1
      log "Docker settings '$(basename "$cand")' is not readable as JSON — skipping it"
      ;;
    *)
      unusable=1
      log "python3 could not inspect Docker settings '$(basename "$cand")' — skipping it"
      ;;
    esac
  done
  if [ -z "$file" ]; then
    if [ "$unusable" = "1" ]; then
      log "no usable Docker Desktop settings file under '$gc' — cannot auto-raise VM memory"
    else
      log "Docker Desktop settings file not found under '$gc' — cannot auto-raise VM memory"
    fi
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

# A4: stop shares the same lock so a stop can never interleave with a
# mid-flight start (pidfile corruption class).
stop_app() {
  if ! launcher_lock_acquire_wait 90; then
    log "another launcher is mid-flight — proceeding with the stop anyway after 90s (stop must always win)"
  fi
  local _rc=0
  stop_app_inner || _rc=$?
  launcher_lock_release
  return $_rc
}

stop_app_inner() {
  # B5: app_alive now verifies the pidfile pid IS our binary (cmdline +
  # start-time plausibility) — a recycled/foreign pid is treated as no-app
  # and is NEVER killed. The pidfile-blind sweep below still finds and
  # stops the REAL app if one is running without a (valid) pidfile.
  if app_alive; then
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
  else
    log "no (valid) pidfile app to stop — checking for a stray app process"
  fi
  rm -f "$APP_PIDFILE"
  # B5: pidfile-blind sweep — Stop must ALWAYS win. Any remaining tickvault
  # release binary (crashed launcher's orphan, foreign pidfile case) is
  # stopped by NAME, so it can never be a stranger's process.
  local stray w
  stray="$(find_running_app)"
  if [ -n "$stray" ]; then
    log "stray tickvault process (pid $stray) survived the pidfile stop — stopping it by name"
    pkill -TERM -f 'target/release/tickvault' 2>/dev/null || true
    for w in $(seq 1 12); do
      pgrep -f 'target/release/tickvault' >/dev/null 2>&1 || break
      sleep 5
    done
    if pgrep -f 'target/release/tickvault' >/dev/null 2>&1; then
      log "stray app ignored the polite stop — forcing"
      pkill -KILL -f 'target/release/tickvault' 2>/dev/null || true
    fi
  fi
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

# ── FIX 9: one-shot probe-then-run (operator interface = TWO buttons only) ──
# A committed dated marker (deploy/local/probe-once.date) makes the NEXT
# "Run tickvault" click run the one-time 100-connection lab probe FIRST,
# then continue into the normal live start automatically. Inert on any
# other day and after the one attempt (local stamp). The operator never
# clicks anything extra.
PROBE_ONCE_MARKER="deploy/local/probe-once.date"
maybe_run_probe_once() {
  local today marker_line="" marker_date="" attempt stamp stamp_exists=0
  today="$(ist_date)"
  if [ -f "$PROBE_ONCE_MARKER" ]; then
    marker_line="$(head -1 "$PROBE_ONCE_MARKER")"
  fi
  # FIX 11: the marker may carry an attempt number ("<date> N"); the stamp
  # is per-attempt so a coordinator bump to attempt 2 re-arms exactly one
  # more probe the same day (attempt 1's stamp stays behind as audit).
  marker_date="$(probe_marker_date "$marker_line")"
  attempt="$(probe_marker_attempt "$marker_line")"
  stamp="data/groww-scale/probe-once.done.${today}.${attempt}"
  [ -f "$stamp" ] && stamp_exists=1
  # Backward compat: a pre-FIX-11 unsuffixed stamp counts as attempt 1.
  if [ "$attempt" = "1" ] && [ -f "data/groww-scale/probe-once.done.${today}" ]; then
    stamp_exists=1
  fi
  if [ "$(probe_once_decision "$marker_date" "$today" "$stamp_exists")" != "run" ]; then
    return 0
  fi
  log "one-time lab marker is dated TODAY (${marker_date}, attempt ${attempt}) — running the 100-connection probe once BEFORE the normal start"
  mkdir -p data/groww-scale
  # Stamp BEFORE the probe: even a crash mid-probe must not re-trigger it —
  # the very next click goes straight to the normal start.
  printf '%s %s probe-once attempt %s\n' "$today" "$(ist_hms)" "$attempt" >"$stamp"
  local probe_start_epoch
  probe_start_epoch=$(date +%s)
  export_compose_creds
  if PROBE_AUTO_STOP_MIN="${PROBE_ONCE_MAX_MIN:-45}" bash scripts/scale-100-probe.sh; then
    log "probe finished — continuing into normal live start"
  else
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
    log "probe did not complete cleanly — overlay cleaned; continuing into normal live start anyway"
  fi
  # FIX 10 A8: honor an operator Stop pressed DURING the probe. The probe's
  # OWN internal stop writes the manual-stop marker within seconds of probe
  # start (inside the grace window); a marker noticeably NEWER than that
  # means the OPERATOR pressed Stop mid-probe — it must stand: rc 1 tells
  # the caller to NOT continue into a live start.
  if [ -f "$MANUAL_STOP_MARKER" ]; then
    local marker_mtime
    marker_mtime=$(stat -f %m "$MANUAL_STOP_MARKER" 2>/dev/null ||
      stat -c %Y "$MANUAL_STOP_MARKER" 2>/dev/null || echo 0)
    if manual_stop_during_probe "$marker_mtime" "$probe_start_epoch"; then
      log "Stop was pressed DURING the probe — honoring it: no live start (double-click Start TickVault when ready)"
      return 1
    fi
  fi
  return 0
}

# ── Subcommand: manual start ────────────────────────────────────────────────

cmd_start() {
  log "== MANUAL START (manual always wins — clearing manual-stop marker) =="
  rm -f "$MANUAL_STOP_MARKER"
  ensure_docker || return 1
  check_docker_vm_memory
  # FIX 10 A6: arm caffeinate BEFORE the (possibly long) probe so Mac sleep
  # cannot freeze the ladder into its hard time cap.
  start_caffeinate
  # FIX 9: one-shot lab probe (inert unless the committed marker is dated
  # today). Runs AFTER docker is up (the probe manages QuestDB itself).
  # FIX 10 A8: rc 1 = the operator pressed Stop DURING the probe — it
  # stands; no auto live-start.
  if ! maybe_run_probe_once; then
    stop_caffeinate
    return 0
  fi
  # The probe's internal stop re-arms the manual-stop marker and disarms
  # caffeinate — clear + re-arm both: this click is a START.
  rm -f "$MANUAL_STOP_MARKER"
  start_caffeinate
  # FIX 10 A3: a window-close / SIGHUP / reboot mid-probe can leak the
  # scale overlay — a NORMAL run must never silently boot the Dhan-OFF
  # scale config.
  if has_scale_overlay config/local.toml; then
    log "leftover scale-test overlay found in config/local.toml — removing it (this is a NORMAL run)"
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
  fi
  ensure_questdb || return 1
  disk_free_ok || true # warn-only on manual start — operator's call
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

# boot_chain_once — the 08:55 robot's boot-critical prerequisites, one
# attempt. rc 1 on any gating failure so cmd_run's B2 bounded retry loop
# can try again (preflight_git + the VM-memory check are advisory and
# never gate).
boot_chain_once() {
  preflight_git
  ensure_docker || return 1
  check_docker_vm_memory
  disk_free_ok || return 1
  ensure_questdb || return 1
  return 0
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
    # B3: recompute the IST date PER ITERATION (mirror the per-write
    # daily_app_log pattern) — the $TODAY snapshot from script start goes
    # stale across IST midnight in this long-lived loop, which blinded the
    # loop to a Stop pressed after midnight.
    manual_stop_active "$MANUAL_STOP_MARKER" "$(ist_date)" && manual=1
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

  # B2: bounded boot retry — a transient failure at 08:55 (network blip,
  # Docker still waking after login, database slow) retries every
  # BOOT_RETRY_INTERVAL_SECS until BOOT_RETRY_DEADLINE_IST instead of
  # killing the whole trading day. Telegram fires ONCE on the first
  # failure; a final give-up page fires if the deadline passes.
  local boot_ok=0 boot_warned=0
  while :; do
    if boot_chain_once; then
      boot_ok=1
      break
    fi
    if ! boot_retry_should_retry "$(secs_until "$BOOT_RETRY_DEADLINE_IST" "$(ist_hms)")"; then
      break
    fi
    if ((boot_warned == 0)); then
      boot_warned=1
      tg "⚠️ Morning start hit a temporary problem (network / Docker / database). Retrying automatically every $((BOOT_RETRY_INTERVAL_SECS / 60)) minutes until $BOOT_RETRY_DEADLINE_IST — no action needed yet."
    fi
    log "boot chain failed — retrying in ${BOOT_RETRY_INTERVAL_SECS}s (deadline $BOOT_RETRY_DEADLINE_IST IST)"
    sleep "$BOOT_RETRY_INTERVAL_SECS"
  done
  if ((boot_ok == 0)); then
    tg "🆘 Morning start could NOT get the basics up (Docker / disk space / database) by $BOOT_RETRY_DEADLINE_IST. Open Docker Desktop and check disk space, then double-click Start TickVault."
    exit 1
  fi
  start_caffeinate

  if [ "$day" = "scale" ]; then
    apply_overlay probe
    start_app "scale day — probe overlay" || exit 1
    tg "🚀 Scale-lab day: app booted with the 2×600 cap-probe. Verdict expected between $PROBE_POLL_START_IST and ~10:05 IST — I will read it and act automatically."

    wait_until_ist "$PROBE_POLL_START_IST"
    local verdict="" waited=0 timeout=$((PROBE_TIMEOUT_MIN * 60))
    while ((waited < timeout)); do
      # B3: fresh IST date inside the polling loop (same rule as the
      # monitor loop — never trust the script-start $TODAY in a loop).
      if manual_stop_active "$MANUAL_STOP_MARKER" "$(ist_date)"; then
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
  # B3: fresh IST date — this check runs AFTER the hours-long monitor loop.
  if ! manual_stop_active "$MANUAL_STOP_MARKER" "$(ist_date)"; then
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
