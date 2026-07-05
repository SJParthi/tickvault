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
# FIX 18 G16: set to 1 by check_docker_vm_memory when the pin above had to
# be clamped to a smaller Docker VM — the scale branch refuses the 100k max
# ladder on a clamped run (an OOM'd database would masquerade as a Groww cap).
QDB_MEM_CLAMPED="${QDB_MEM_CLAMPED:-0}"
# FIX 18 G1: every docker CLI call is bounded — a wedged daemon (mid
# auto-update, half-dead socket after a hard Mac sleep/wake) previously hung
# the single-threaded monitor loop forever: no 15:35 EOD stop, no crash
# relaunch, no database self-heal, and the stuck run's lock blocked the next
# morning's fire. Short timeout for probes; long for compose up / restart /
# image pull (real work, still bounded).
DOCKER_CMD_TIMEOUT_SECS="${DOCKER_CMD_TIMEOUT_SECS:-15}"
DOCKER_LONG_TIMEOUT_SECS="${DOCKER_LONG_TIMEOUT_SECS:-300}"
# FIX 18 G13: network git ops (fetch) are bounded too — a captive/black-hole
# Wi-Fi at 08:55 previously HUNG the boot (a hang is not a failure, so the
# graceful offline fallback never fired).
GIT_NET_TIMEOUT_SECS="${GIT_NET_TIMEOUT_SECS:-60}"
# FIX 18 G8/G14: keep-awake FLOOR. A Start after 15:45 IST previously armed
# ZERO caffeinate (secs_until returned 0) — the evening cold release build /
# weekend probe ladder ran with no idle-sleep protection. The keep-awake is
# now max(until CAFFEINATE_UNTIL_IST, this floor); the EOD stop still
# disarms it early on trading days.
CAFFEINATE_MIN_SECS="${CAFFEINATE_MIN_SECS:-14400}"

APP_PIDFILE="$AUTOPILOT_DIR/app.pid"
CAFF_PIDFILE="$AUTOPILOT_DIR/caffeinate.pid"
LOCKDIR="$AUTOPILOT_DIR/autopilot.lock.d"
# FIX 18 G5: the manual-start background monitor's pidfile (spawned by
# cmd_start when no 08:55 robot is covering the day).
MONITOR_PIDFILE="$AUTOPILOT_DIR/monitor.pid"

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
# → idle | manual_standdown | relaunch | alert | idle_alerted
# (the monitor-loop brain). Manual ALWAYS wins: a manual stop is never
# fought with a relaunch. FIX 16 (F4): after the ONE relaunch and the ONE
# alert, the state is idle_alerted — the loop stays quiet instead of
# paging every 60s until 15:35 (edge-triggered alerts only, audit Rule 4;
# the old "# alert once, then idle" comment claimed a latch that never
# existed — a double-crash produced ~300+ identical pages).
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
  if ((relaunches < 1)); then
    echo relaunch
  elif ((relaunches < 2)); then
    echo alert
  else
    echo idle_alerted
  fi
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

# docker_settings_mem_get <settings-json-file> <key> — FIX 17: print the
# CURRENT integer value of the memory key (read-only), rc 1 with nothing
# printed when the file/key is absent or the value is not an integer. Used
# to make the memory-preference hint idempotent (never rewrite, never loop).
docker_settings_mem_get() {
  [ -f "${1:-}" ] || return 1
  python3 - "$1" "${2:-}" <<'PY'
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
except Exception:
    sys.exit(1)
v = d.get(sys.argv[2]) if isinstance(d, dict) else None
if isinstance(v, bool) or not isinstance(v, int):
    sys.exit(1)
print(v)
PY
}

# docker_mem_hint_decision <current-mib-or-empty> <target-mib> — FIX 17 pure
# no-loop guard: "skip" when the settings file ALREADY carries a numeric
# value >= target (the hint was written on an earlier run — rewriting every
# launch is what made the FIX-7 raise re-fire forever on Docker Desktop
# 4.80, which ignores the file edit), else "write". Garbage target → "skip"
# (fail-quiet: never write on uncertain input). Total: always prints one
# word, always rc 0 — safe under set -e.
docker_mem_hint_decision() {
  local cur="${1:-}" tgt="${2:-}"
  case "$tgt" in '' | *[!0-9]*)
    echo skip
    return 0
    ;;
  esac
  case "$cur" in '' | *[!0-9]*)
    echo write
    return 0
    ;;
  esac
  if [ "$cur" -ge "$tgt" ]; then echo skip; else echo write; fi
  return 0
}

# ── FIX 19: raise Docker VM memory at its NATURAL cold start ────────────────
# Theory of the FIX-7/17 failures (labeled Assumed — consistent with all the
# live evidence, not directly proven): Docker Desktop keeps its settings in
# memory while running and FLUSHES them to settings-store.json on exit. Our
# earlier MemoryMiB writes happened while Docker was RUNNING, then the
# quit/relaunch (or the operator's later quit) overwrote the edit — so the
# file "ignored" the change. The fix: write the file ONLY when the daemon is
# confirmed DOWN and Docker.app is NOT running, THEN launch Docker.app — a
# cold start reads the file, so every natural start (reboot, morning, first
# run of the day) comes up with the raised memory. Never touch the file
# while Docker runs; never quit a running Docker. The post-start VM-memory
# log line is the proof channel: the NEXT run's log shows whether it stuck.

# docker_mem_target_mib <host_mem_bytes> → the default Docker VM memory
# target for this host: 12288 MiB (12 GB) on a >= 32 GB Mac (the operator's
# is 48 GB — the clamp math then gives the database 8g+ naturally), 10240
# otherwise. Garbage/empty input → 10240 (conservative). Pure.
docker_mem_target_mib() {
  local b="${1:-}"
  case "$b" in '' | *[!0-9]*)
    echo 10240
    return 0
    ;;
  esac
  if [ "$b" -ge $((32 * 1024 * 1024 * 1024)) ]; then echo 12288; else echo 10240; fi
  return 0
}

# docker_cpu_target_cores <host_cores> → a matching Docker VM CPU count for
# the raised memory: hosts with >= 12 cores give the VM (host - 4) cores
# (the operator's 14-core Mac → 10 — plenty for the 100k ladder while macOS
# keeps 4); smaller hosts get half, floor 2. Garbage → 0 (caller skips). Pure.
docker_cpu_target_cores() {
  local c="${1:-}"
  case "$c" in '' | *[!0-9]* | 0)
    echo 0
    return 0
    ;;
  esac
  if [ "$c" -ge 12 ]; then
    echo $((c - 4))
  else
    local half=$((c / 2))
    if [ "$half" -lt 2 ]; then half=2; fi
    echo "$half"
  fi
  return 0
}

# cold_start_mem_write_decision <daemon_up 0|1> <app_running 0|1> → write|skip.
# The settings file may be written ONLY when the daemon is DOWN and the
# Docker.app process is NOT running (see the Assumed flush-on-exit theory
# above). Anything else — including garbage input — is skip (fail-quiet:
# never risk a write that a running Docker will overwrite or race). Pure.
cold_start_mem_write_decision() {
  if [ "${1:-1}" = "0" ] && [ "${2:-1}" = "0" ]; then echo write; else echo skip; fi
  return 0
}

# ── FIX 19 item 3: probe-prep stop wording ──────────────────────────────────
# stop_notice_mode <probe_prep_flag> → probe_prep|manual. The probe wrapper's
# internal pre-probe stop used to send the manual-stop Telegram ("Autopilot
# will NOT auto-restart today — double-click Start TickVault to resume"),
# which is terrifying to read seconds after clicking Run. When the stop is
# part of the probe's own runway-clearing (AUTOPILOT_PROBE_PREP=1 exported by
# the wrapper), the message says so and does NOT claim autopilot stands down
# (the probe path tolerates the marker via its grace window anyway). Pure.
stop_notice_mode() {
  case "${1:-}" in 1 | true | yes) echo probe_prep ;; *) echo manual ;; esac
  return 0
}

# ── FIX 19 item 7: guaranteed per-feed Telegram status pings ────────────────
# The operator's 11:01 live run showed ZERO feed-level Telegram messages —
# only launcher messages. Root cause (main-lane bug, noted for a crate PR):
# the app's Groww boot-ping notifier slot is filled ONLY on the fast-boot
# arm (main.rs), so a slow-boot / groww-only (max-smoke) run defers the
# "connected — awaiting first tick" ping forever; and a Dhan that is OFF is
# announced by NOBODY. The launcher now guarantees one Telegram per feed
# after every launch by reading the app's public /api/feeds/health endpoint.

# feed_ping_text <feed_name> <enabled true|false> <verdict> <connected true|false> <subscribed> [ticks] [market_open 0|1]
# → one plain-English Telegram line for the feed. An OFF feed is ANNOUNCED
# (silence was the bug); garbage subscribed counts render as 0. Pure.
# FIX 20 task 2: "data flowing" was claimed unconditionally on the ok
# verdict — during a CLOSED market with 0 ticks that is a FALSE claim
# (audit Rule 11, no false-OK). The wording is now state-based on the
# tick count (arg 6) + market-open flag (arg 7); omitted args fail SAFE
# to the honest "awaiting first tick" wording, never to "data flowing".
feed_ping_text() {
  local name="${1:-feed}" enabled="${2:-}" verdict="${3:-unknown}" connected="${4:-}" sub="${5:-0}"
  local ticks="${6:-0}" market_open="${7:-0}"
  case "$sub" in '' | *[!0-9]*) sub=0 ;; esac
  case "$ticks" in '' | *[!0-9]*) ticks=0 ;; esac
  case "$market_open" in 0 | 1) ;; *) market_open=0 ;; esac
  if [ "$enabled" != "true" ]; then
    echo "⏸️ ${name}: switched OFF for this run (expected in a lab/max-smoke session — not an error)"
    return 0
  fi
  case "$verdict" in
  ok)
    if [ "$ticks" -gt 0 ]; then
      echo "✅ ${name}: connected, ${sub} instruments subscribed, data flowing"
    elif [ "$market_open" = "1" ]; then
      echo "⚠️ ${name}: connected, ${sub} subscribed — NO ticks yet (investigating if it persists)"
    else
      echo "✅ ${name}: connected, ${sub} subscribed — awaiting first tick (market closed)"
    fi
    ;;
  degraded)
    echo "⚠️ ${name}: connected but DEGRADED — ${sub} instruments subscribed; watch the next messages"
    ;;
  down)
    echo "🆘 ${name}: NOT connected (failed: down) — the app keeps retrying automatically; if this persists 10 minutes, double-click Stop TickVault then Start TickVault"
    ;;
  *)
    if [ "$connected" = "true" ]; then
      echo "✅ ${name}: connected, ${sub} instruments subscribed, awaiting ticks"
    else
      echo "⏳ ${name}: enabled, still connecting (${sub} subscribed so far) — a follow-up message will confirm"
    fi
    ;;
  esac
  return 0
}

# feed_rows_settled <rows> → rc 0 when every ENABLED feed row is connected or
# carries a non-unknown verdict (nothing left to wait for), rc 1 otherwise
# (keep polling). Rows are "name|enabled|verdict|connected|subscribed" lines.
# Empty input → rc 1. Pure.
feed_rows_settled() {
  [ -n "${1:-}" ] || return 1
  local name enabled verdict connected sub
  while IFS='|' read -r name enabled verdict connected sub; do
    [ -n "$name" ] || continue
    if [ "$enabled" = "true" ] && [ "$connected" != "true" ] && [ "$verdict" = "unknown" ]; then
      return 1
    fi
  done <<<"$1"
  return 0
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

# tsv_rows <file> → the file's line count as a clean integer; a MISSING file
# is 0 with NO stderr noise. FIX 19 item 2: `wc -l <"$file"` on an absent
# file makes the SHELL print a raw "No such file or directory" line (the
# input redirection fails before wc's own 2>/dev/null applies, and the
# operator saw exactly two of those during the live probe run). The [ -f ]
# guard means the redirection is never attempted on an absent file.
tsv_rows() {
  [ -f "${1:-}" ] || {
    echo 0
    return 0
  }
  tsv_sentinel "$(wc -l <"$1" 2>/dev/null || echo 0)"
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

# ── FIX 15: final pre-Monday hardening — pure helpers ───────────────────────

# fd_limit_target <desired> <hard> → the soft open-files value to request:
# desired clamped to the hard limit ("unlimited" hard passes desired
# through). Garbage/empty inputs → rc 1 (caller leaves limits as-is).
# FIX 15.1: the 100-connection fleet exhausts the macOS default 256 fds.
fd_limit_target() {
  local desired="${1:-}" hard="${2:-}"
  case "$desired" in '' | *[!0-9]*) return 1 ;; esac
  if [ "$hard" = "unlimited" ]; then
    echo "$desired"
    return 0
  fi
  case "$hard" in '' | *[!0-9]*) return 1 ;; esac
  if ((desired > hard)); then echo "$hard"; else echo "$desired"; fi
}

# raise_fd_limit — best-effort soft-limit raise to FD_LIMIT_DESIRED (clamped
# to the hard limit). Plain log either way; NEVER gates a start.
FD_LIMIT_DESIRED="${FD_LIMIT_DESIRED:-4096}"
raise_fd_limit() {
  local soft hard target
  soft=$(ulimit -Sn 2>/dev/null || echo "")
  hard=$(ulimit -Hn 2>/dev/null || echo "")
  [ "$soft" = "unlimited" ] && return 0
  if ! target=$(fd_limit_target "$FD_LIMIT_DESIRED" "$hard"); then
    log "open-files limit probe unreadable (soft=${soft:-?} hard=${hard:-?}) — leaving limits as-is"
    return 0
  fi
  case "$soft" in '' | *[!0-9]*) soft=0 ;; esac
  if ((soft >= target)); then
    # FIX 16 (F17): honour the "plain log either way" contract on the
    # third outcome — the hard limit is pre-lowered (managed/MDM Mac) so
    # the raise is impossible AND the fleet budget is short. Without this
    # line the first symptom is EMFILE at rungs 80-100, indistinguishable
    # from a genuine Groww-side cap in the recorded verdict.
    if ((target < FD_LIMIT_DESIRED)); then
      log "open-files hard limit is only ${hard:-?} (< ${FD_LIMIT_DESIRED} wanted) — cannot raise further; a big fleet may hit 'too many open files'"
    fi
    return 0
  fi
  if ulimit -n "$target" 2>/dev/null; then
    log "raised open-files limit ${soft} → ${target} (the 100-connection fleet needs more than the macOS default 256)"
  else
    log "could not raise the open-files limit (soft=${soft}, wanted ${target}, hard=${hard:-?}) — a big fleet may hit 'too many open files'"
  fi
  return 0
}

# probe_launch_failed <rows_before> <rows_after> → 0 when the probe wrote
# ZERO new ladder evidence rows (it never launched at all — distinct from a
# partial run). Garbage → rc 1 (fail-safe: never un-burn an attempt on
# unreadable evidence). FIX 15.6.
probe_launch_failed() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  case "${2:-}" in '' | *[!0-9]*) return 1 ;; esac
  (("$2" <= "$1"))
}

# probe_rc_class <wrapper-rc> → complete | interrupted | never_launched |
# other. FIX 16 (F6/F7): the scale-100-probe wrapper's exit-code contract —
# 130/143 = INT/TERM mid-run (Groww connections may already have been
# opened → the one-shot attempt MUST stay burned); 20 = zero evidence rows
# with no signal (never launched → safe to un-burn); 0 = natural
# completion; anything else = partial/unknown (evidence decides).
probe_rc_class() {
  case "${1:-}" in
  0) echo complete ;;
  130 | 143) echo interrupted ;;
  20) echo never_launched ;;
  *) echo other ;;
  esac
}

# tg_latch_expired <now_epoch> <disabled_at_epoch> <retry_secs> → 0 when the
# Telegram-disabled latch is old enough to retry the credential read.
# Garbage disabled_at → EXPIRED (retry rather than stay dead all day);
# garbage now → not expired. FIX 15.7.
tg_latch_expired() {
  case "${2:-}" in '' | *[!0-9]*) return 0 ;; esac
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  (("$1" - "$2" >= "${3:-3600}"))
}

# vm_disk_high <pct> <threshold> → 0 when pct is a plain integer above the
# threshold. Garbage/empty pct → rc 1 (no warning on unreadable probe).
# FIX 15.8: the host df cannot see Docker's VM virtual disk filling.
vm_disk_high() {
  case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
  (("$1" > "${2:-85}"))
}

# docker_vm_disk_pct → integer use% of the Docker VM's own filesystem, read
# THROUGH the database container (its / IS the VM overlay fs — the host df
# cannot see it). Empty on any failure — GUARANTEED rc 0 (FIX 16 F9): under
# `set -euo pipefail` a dead tv-questdb made `docker exec` fail, pipefail
# carried rc 1 through the pipeline into `vm_pct=$(docker_vm_disk_pct)`,
# and errexit killed the WHOLE monitor loop silently — no relaunch, no
# questdb heal, no EOD stop — at the next 30-min disk recheck. The exact
# dead-container scenario the heal exists for must never abort the probe.
docker_vm_disk_pct() {
  # FIX 18 G1: bounded via dk — a wedged daemon made this exec BLOCK forever
  # inside the monitor loop's 30-min disk recheck (runtime-only: dk is
  # defined in the runtime section; this fn is never invoked in lib mode).
  dk exec tv-questdb df -Pk / 2>/dev/null | awk 'NR==2 {gsub(/%/, "", $5); print $5}' || true
}

# dhan_enabled_from_files <file...> → the effective dhan_enabled value:
# FIRST file carrying the key wins (pass local.toml before base.toml);
# handles the no-spaces `dhan_enabled=true` variant and trailing comments;
# no file carries it → "false". FIX 15.9.
dhan_enabled_from_files() {
  local f v
  for f in "$@"; do
    v=$(grep -E '^[[:space:]]*dhan_enabled[[:space:]]*=' "$f" 2>/dev/null |
      tail -1 | sed -E 's/.*=[[:space:]]*//; s/[[:space:]#].*$//')
    if [ -n "$v" ]; then
      echo "$v"
      return 0
    fi
  done
  echo false
}

# questdb_session_healthy — cheap per-iteration database liveness: the
# tv-questdb container must be running (owner verdict) AND answer one short
# HTTP ping. FIX 15.2 (the docker DAEMON can be alive while the container
# is dead — OOM-kill, crash — previously undetected all day).
questdb_session_healthy() {
  [ "$(questdb_owner_verdict "$(questdb_container_state)")" = "ok" ] || return 1
  curl -sf -m 3 -G "$QUESTDB_HTTP/exec" --data-urlencode "query=select 1" >/dev/null 2>&1
}

# aws_prod_running_check — FIX 15.9 (belt-and-braces): the AWS prod box
# RUNNING while this Mac boots Dhan-enabled = dual-instance token fight
# (the safety lock makes it fail-closed, but the local day is lost). Warn
# loudly; never gates.
PROD_INSTANCE_ID="${PROD_INSTANCE_ID:-i-0b956d0209231a48b}"
aws_prod_running_check() {
  command -v aws >/dev/null 2>&1 || return 0
  [ "$(dhan_enabled_from_files config/local.toml config/base.toml)" = "true" ] || return 0
  local state
  state=$(aws ec2 describe-instances --region ap-south-1 \
    --instance-ids "$PROD_INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].State.Name' --output text \
    --no-cli-pager --cli-connect-timeout 5 --cli-read-timeout 10 2>/dev/null || true)
  if [ "$state" = "running" ]; then
    log "AWS prod box $PROD_INSTANCE_ID is RUNNING while this Mac is Dhan-enabled — dual-instance risk"
    tg "⚠️ The AWS trading machine is RUNNING right now while this Mac is also set to use the Dhan feed. Only ONE machine can hold the Dhan session — the safety lock will stop the second one. Pause the AWS machine (or turn Dhan off locally) before market open."
  fi
  return 0
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

# ── FIX 18 pure helpers (real-environment permutation batch, 2026-07-05) ───

# timeout_runner <have_timeout 0|1> <have_gtimeout 0|1> <have_perl 0|1>
# → timeout|gtimeout|perl|none. G1: macOS ships NO `timeout` binary —
# coreutils provides gtimeout; the perl-alarm shim (alarm survives exec)
# is the always-present fallback; none = run unbounded (logged once).
timeout_runner() {
  if [ "${1:-0}" = "1" ]; then
    echo timeout
  elif [ "${2:-0}" = "1" ]; then
    echo gtimeout
  elif [ "${3:-0}" = "1" ]; then
    echo perl
  else
    echo none
  fi
}

# timed_out_rc <rc> → 0 when the rc means the wrapped command was KILLED by
# the timeout (124 = timeout/gtimeout convention; 142 = 128+SIGALRM from
# the perl shim). Any other rc — including garbage — is NOT a timeout.
timed_out_rc() {
  case "${1:-}" in 124 | 142) return 0 ;; *) return 1 ;; esac
}

# docker_start_failure_hint <app_running 0|1> <settings_exists 0|1>
# → first_launch | app_stuck | not_running. G2/G6/G11: a FRESH Docker
# Desktop's first-ever launch blocks the engine behind the Subscription
# Service Agreement / onboarding modal — launching Docker.app shows the
# window but the daemon never starts until a human clicks Accept. App
# running with NO settings store on disk = never initialized = modal is up.
docker_start_failure_hint() {
  if [ "${1:-0}" != "1" ]; then
    echo not_running
    return 0
  fi
  if [ "${2:-0}" = "1" ]; then echo app_stuck; else echo first_launch; fi
}

# caffeinate_secs <target HH:MM> <now HH:MM:SS> <floor_secs> → keep-awake
# seconds: max(secs-until-target, floor). G8/G14 (see CAFFEINATE_MIN_SECS).
# Garbage floor → 0 (falls back to the plain until-target behavior).
caffeinate_secs() {
  local until floor="${3:-0}"
  until=$(secs_until "${1:-}" "${2:-}")
  case "$floor" in '' | *[!0-9]*) floor=0 ;; esac
  if ((until > floor)); then echo "$until"; else echo "$floor"; fi
}

# pull_failure_class <docker pull output> → rate_limit|offline|other. G13:
# a Docker Hub anonymous-pull 429 or an offline/captive network on the
# first-ever image pull was misdiagnosed as "Docker not running".
pull_failure_class() {
  case "${1:-}" in
  *toomanyrequests* | *"429"* | *"rate limit"* | *"pull rate"*) echo rate_limit ;;
  *"no such host"* | *"lookup "* | *"connection refused"* | *"network is unreachable"* | *"i/o timeout"* | *"TLS handshake timeout"* | *"request canceled"*) echo offline ;;
  *) echo other ;;
  esac
}

# run_lock_takeover <holder_alive 0|1> <lock_date> <today> → exit|steal|reclaim
# G1 compounding: a WEDGED previous-day run (hung on a dead docker socket)
# held the run lock forever — every next-morning fire then saw the pid
# "alive" and exited, losing the day. A live SAME-day holder is a genuine
# duplicate (exit); a live holder whose lock date is a DIFFERENT day is
# yesterday's stuck run (steal: stop it + take over); a dateless live
# holder cannot be proven stale (exit — conservative); dead → reclaim.
run_lock_takeover() {
  if [ "${1:-0}" != "1" ]; then
    echo reclaim
    return 0
  fi
  if [ -z "${2:-}" ] || [ "${2:-}" = "${3:-}" ]; then echo exit; else echo steal; fi
}

# manual_monitor_needed <run_lock_alive 0|1> <monitor_alive 0|1>
#                       <secs_until_eod> → spawn|skip. G5: a manual Start
# with no 08:55 robot alive previously ran the whole day with NO monitor —
# no 15:35 EOD stop, no crash relaunch, no mid-session disk/database
# self-heal. Spawn only when nothing else covers the day and the EOD stop
# is still ahead (an evening session runs until the operator presses Stop).
manual_monitor_needed() {
  [ "${1:-0}" = "0" ] || {
    echo skip
    return 0
  }
  [ "${2:-0}" = "0" ] || {
    echo skip
    return 0
  }
  case "${3:-}" in
  '' | *[!0-9]*)
    echo skip
    return 0
    ;;
  esac
  if (("${3}" > 0)); then echo spawn; else echo skip; fi
}

# oomkilled_is_true <docker inspect .State.OOMKilled output> → rc 0 when the
# container was killed by its OWN memory limit. G16: an OOM-killed database
# must be reported as OOM (raise Docker memory), never as a generic stall
# or a Groww cap.
oomkilled_is_true() { [ "$(printf '%s' "${1:-}" | tr -d '[:space:]')" = "true" ]; }

# scale_max_gate <qdb_mem_clamped 0|1> → max|skip_max. G16: the 100k max
# ladder was pinned QDB_MEM_LIMIT=8g precisely so it gets its full budget;
# a clamped (smaller Docker VM) run silently defeated the pin and told the
# operator "works fine" — then any OOM pressure would be misread as a
# Groww per-account cap. A clamped run skips the max rung.
scale_max_gate() { if [ "${1:-0}" = "1" ]; then echo skip_max; else echo max; fi; }

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
TG_DISABLED_AT=""
TG_RETRY_SECS="${TG_RETRY_SECS:-3600}"
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
  TG_DISABLED_AT=$(date +%s)
  log "Telegram unavailable (tried env path(s): ${candidates// /, } under /tickvault/<env>/telegram/ in ap-south-1 — no internet, no AWS credentials, or missing parameter). Messages go to this log only; the credential read is retried automatically after $((TG_RETRY_SECS / 60)) minutes (FIX 15.7)."
}

tg() { # best-effort Telegram; the log always carries the message
  log "TELEGRAM: $*"
  tg_init
  if ((TG_DISABLED)); then
    # FIX 15.7: time-based re-arm — one transient SSM failure at the first
    # message must not kill Telegram for the whole 08:55–15:35 run. After
    # TG_RETRY_SECS the credential read is attempted again.
    if tg_latch_expired "$(date +%s)" "${TG_DISABLED_AT:-}" "$TG_RETRY_SECS"; then
      log "Telegram retry window reached — re-attempting the credential read"
      TG_INIT_DONE=0
      TG_DISABLED=0
      tg_init
    fi
    if ((TG_DISABLED)); then return 0; fi
  fi
  bash scripts/notify-telegram.sh "💻 LOCAL autopilot — $*" >>"$(current_log)" 2>&1 || true
}

# ── FIX 18 G1: bounded external commands ────────────────────────────────────
# with_timeout <secs> <cmd...> — run cmd under a hard wall-clock timeout.
# Runner picked once per process by timeout_runner (macOS: gtimeout or the
# perl-alarm shim — alarm(2) survives execve, so the exec'd command gets
# SIGALRM → rc 142). No runner at all → run unbounded, logged once.
WITH_TIMEOUT_RUNNER=""
with_timeout() {
  local secs="$1"
  shift
  if [ -z "$WITH_TIMEOUT_RUNNER" ]; then
    WITH_TIMEOUT_RUNNER=$(timeout_runner \
      "$(command -v timeout >/dev/null 2>&1 && echo 1 || echo 0)" \
      "$(command -v gtimeout >/dev/null 2>&1 && echo 1 || echo 0)" \
      "$(command -v perl >/dev/null 2>&1 && echo 1 || echo 0)")
    if [ "$WITH_TIMEOUT_RUNNER" = "none" ]; then
      log "no timeout/gtimeout/perl available — external commands run UNBOUNDED (install coreutils for gtimeout)"
    fi
  fi
  case "$WITH_TIMEOUT_RUNNER" in
  timeout) timeout "$secs" "$@" ;;
  gtimeout) gtimeout "$secs" "$@" ;;
  perl) perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@" ;;
  *) "$@" ;;
  esac
}

# dk [--long] <docker args...> — EVERY docker CLI call goes through here
# (G1): a wedged daemon (mid auto-update, half-dead vpnkit socket after a
# hard sleep/wake) must make the probe return "unknown" instead of hanging
# the single-threaded monitor loop past the 15:35 EOD stop. A timeout pages
# ONCE per wedged episode via a MARKER-FILE latch (a variable latch dies in
# command-substitution subshells like $(docker_vm_disk_pct)); any later
# successful docker call re-arms it.
DOCKER_WEDGED_MARK="$AUTOPILOT_DIR/docker-wedged-paged"
dk() {
  local secs="$DOCKER_CMD_TIMEOUT_SECS" rc=0
  if [ "${1:-}" = "--long" ]; then
    secs="$DOCKER_LONG_TIMEOUT_SECS"
    shift
  fi
  with_timeout "$secs" docker "$@" || rc=$?
  if timed_out_rc "$rc"; then
    log "docker command timed out after ${secs}s (docker ${1:-?} ...) — daemon wedged? treating this probe as unknown and moving on"
    if [ ! -f "$DOCKER_WEDGED_MARK" ]; then
      touch "$DOCKER_WEDGED_MARK" 2>/dev/null || true
      tg "⚠️ Docker stopped answering (a command hung and was cut off safely). The run keeps going and keeps re-checking — if Docker Desktop is updating or shows a dialog, open it once and let it finish. The daily 3:35 PM stop is unaffected."
    fi
  elif [ "$rc" = "0" ] && [ -f "$DOCKER_WEDGED_MARK" ]; then
    rm -f "$DOCKER_WEDGED_MARK" 2>/dev/null || true
    log "docker answering again — wedged-daemon page re-armed"
  fi
  return $rc
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
  if [ -n "${TV_QUESTDB_PG_USER:-}" ] && [ -n "${TV_QUESTDB_PG_PASSWORD:-}" ]; then
    COMPOSE_CREDS_DONE=1
    return 0
  fi
  # FIX 18 G3/G7: resolve the SSM environment the way the APP does
  # (TV_ENVIRONMENT → ENVIRONMENT → prod, via tg_env_resolve — the FIX 8.1
  # pattern) and fall back to prod like tg_init. The previous dev default
  # read the nonexistent /tickvault/dev/questdb/* on the operator's
  # prod-seeded account, so a fresh-clone cold container could never be
  # created (the compose ${TV_QUESTDB_PG_PASSWORD:?} guard aborted).
  local env candidates cand u="" p=""
  env="$(tg_env_resolve "${TV_ENVIRONMENT:-}" "${ENVIRONMENT:-}")"
  candidates="$env"
  [ "$env" != "prod" ] && candidates="$env prod"
  if command -v aws >/dev/null 2>&1; then
    for cand in $candidates; do
      u=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
        --name "/tickvault/${cand}/questdb/pg-user" \
        --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
      p=$(aws ssm get-parameter --region ap-south-1 --with-decryption \
        --name "/tickvault/${cand}/questdb/pg-password" \
        --query Parameter.Value --output text --no-cli-pager 2>/dev/null || true)
      if [ -n "$u" ] && [ "$u" != "None" ] && [ -n "$p" ] && [ "$p" != "None" ]; then
        export TV_QUESTDB_PG_USER="$u" TV_QUESTDB_PG_PASSWORD="$p" # secret-scan-ignore: values read from AWS SSM above, never hardcoded
        # FIX 18 G12: latch ONLY on success. A transient 08:55 SSM/network
        # blip previously baked the empty result in for the whole process,
        # defeating the 5-minute boot-retry loop built for exactly that
        # blip (mirror of the tg_init FIX 15.7 pattern).
        COMPOSE_CREDS_DONE=1
        log "QuestDB compose credentials loaded from /tickvault/${cand}/questdb/* (region ap-south-1)"
        return 0
      fi
    done
  fi
  log "QuestDB credentials unreadable from SSM (tried: ${candidates// /, } under /tickvault/<env>/questdb/) — the database container can still START if it already exists, but compose cannot (re)create it; the read is retried on the next boot-chain attempt (latch only on success — FIX 18 G12)"
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

# FIX 18 G1: docker info bounded via dk (a wedged daemon BLOCKS here rather
# than erroring — previously this froze the monitor loop indefinitely).
docker_alive() { dk info >/dev/null 2>&1; }

# FIX 19 addendum: raise-at-natural-start. Called ONLY from ensure_docker's
# daemon-is-down path, immediately BEFORE Docker.app is launched. Writes the raised
# MemoryMiB (+ a matching Cpus when the key already exists) into the settings
# file so THIS cold start reads it — never touches the file while Docker
# runs, never quits a running Docker (the Assumed flush-on-exit theory above:
# writing while Docker runs is overwritten when it exits). Best-effort: every
# failure path logs and returns 0 so the launch always proceeds.
raise_docker_mem_at_cold_start() {
  [ "$(uname -s)" = "Darwin" ] || return 0
  [ -z "${CI:-}" ] || return 0
  local app_running=0
  if pgrep -f "/Applications/Docker.app" >/dev/null 2>&1; then app_running=1; fi
  # Daemon is confirmed down at this call site (ensure_docker's docker_alive
  # already returned false), so daemon_up=0.
  if [ "$(cold_start_mem_write_decision 0 "$app_running")" != "write" ]; then
    log "Docker.app process is already running (engine still coming up) — leaving its settings file alone (a write now would be overwritten when Docker exits)"
    return 0
  fi
  local host_bytes target
  host_bytes=$(sysctl -n hw.memsize 2>/dev/null || true)
  target="${TV_DOCKER_VM_MEM_MIB:-}"
  case "$target" in '' | *[!0-9]*) target=$(docker_mem_target_mib "$host_bytes") ;; esac
  # Locate the settings file + memory key (store first, then legacy).
  local gc="$HOME/Library/Group Containers/group.com.docker"
  local file="" key="" cand rc
  for cand in "$gc/settings-store.json" "$gc/settings.json"; do
    key=$(docker_settings_mem_key "$cand") && rc=0 || rc=$?
    case "$rc" in
    0 | 10)
      file="$cand"
      break
      ;;
    *) : ;;
    esac
  done
  if [ -z "$file" ]; then
    log "no Docker settings file found yet (first-time Docker Desktop setup) — nothing to raise; launching as-is"
    return 0
  fi
  local current
  current=$(docker_settings_mem_get "$file" "$key" 2>/dev/null || true)
  if [ -n "$current" ] && [ "$current" -ge "$target" ] 2>/dev/null; then
    log "Docker VM memory already set to ${current} MiB (>= target ${target}) in $(basename "$file") — no write needed before this cold start"
    return 0
  fi
  # Backup first — the settings file carries every Docker Desktop setting.
  cp "$file" "${file}.tickvault-bak" 2>/dev/null || true
  if docker_settings_set_mem "$file" "$key" "$target"; then
    log "COLD-START RAISE: wrote Docker VM memory ${target} MiB (was ${current:-unset}) into $(basename "$file") BEFORE launching Docker — a cold start reads this file, so Docker should come up with the raised memory (backup: $(basename "$file").tickvault-bak)"
  else
    log "could not write Docker VM memory into $(basename "$file") — launching Docker at its current setting"
    return 0
  fi
  # Matching Cpus — only when the file ALREADY carries a Cpus key (never
  # insert a CPU setting Docker did not have) and only upward.
  local host_cores cpu_key cpu_cur cpu_target
  host_cores=$(sysctl -n hw.ncpu 2>/dev/null || true)
  cpu_target=$(docker_cpu_target_cores "$host_cores")
  if [ "$cpu_target" -gt 0 ] 2>/dev/null; then
    for cpu_key in Cpus cpus; do
      cpu_cur=$(docker_settings_mem_get "$file" "$cpu_key" 2>/dev/null || true)
      if [ -n "$cpu_cur" ]; then
        if [ "$cpu_cur" -lt "$cpu_target" ] 2>/dev/null; then
          if docker_settings_set_mem "$file" "$cpu_key" "$cpu_target"; then
            log "COLD-START RAISE: Docker VM CPUs ${cpu_cur} → ${cpu_target} (host has ${host_cores} cores)"
          fi
        else
          log "Docker VM CPUs already ${cpu_cur} (target ${cpu_target}) — unchanged"
        fi
        break
      fi
    done
  fi
  return 0
}

ensure_docker() {
  # FIX 18 G10: Docker Desktop 4.x per-user installs put the docker CLI in
  # ~/.docker/bin (and Docker.app's Resources/bin) WITHOUT the privileged
  # /usr/local/bin symlink — the launchd plist PATH missed those, so the
  # 08:55 robot said "NOT INSTALLED" while a Terminal run (login shell,
  # ~/.zprofile PATH) worked. Probe the known locations before declaring
  # docker missing, and say "not found" (not "not installed") honestly.
  if ! command -v docker >/dev/null 2>&1; then
    local dkdir
    for dkdir in "$HOME/.docker/bin" "/Applications/Docker.app/Contents/Resources/bin"; do
      if [ -x "$dkdir/docker" ]; then
        PATH="$PATH:$dkdir"
        export PATH
        log "docker CLI found at $dkdir (was not on PATH) — added for this run"
        break
      fi
    done
  fi
  if ! command -v docker >/dev/null 2>&1; then
    tg "🆘 The docker command was NOT FOUND on this Mac (checked the usual places including Docker Desktop's own folders). If Docker Desktop is installed, open it once and let it finish setup; otherwise install it. Then double-click Start TickVault."
    return 1
  fi
  if docker_alive; then return 0; fi
  log "Docker daemon not running — attempting to launch Docker.app"
  if [ "$(uname -s)" = "Darwin" ]; then
    # FIX 19 addendum: the daemon is DOWN — the one safe moment to write the
    # raised VM memory into the settings file (see raise_docker_mem_at_cold_start).
    raise_docker_mem_at_cold_start || true
    open -a Docker >>"$(current_log)" 2>&1 || true
  fi
  local i
  for i in $(seq 1 24); do
    sleep 5
    if docker_alive; then
      log "Docker daemon is up (waited $((i * 5))s)"
      # FIX 19 addendum: proof channel — this line is how the NEXT run's log
      # shows whether the cold-start raise actually stuck.
      local vm_probe
      vm_probe=$(dk info --format '{{.MemTotal}}' 2>/dev/null || true)
      log "Docker VM memory after start: ${vm_probe:-unknown} bytes"
      return 0
    fi
  done
  # FIX 18 G2/G6/G11: a FRESH Docker Desktop's first launch blocks the
  # engine behind the Subscription Service Agreement / onboarding modal —
  # launching Docker.app shows the window but the daemon never starts until a
  # human clicks Accept. Detect the never-initialized state (app process up,
  # no settings store on disk) and say exactly that instead of the generic
  # "start Docker manually". Pre-seeding the accepted-license flag is NOT
  # done: Docker Desktop 4.80 ignores settings-file edits (proven live
  # 2026-07-05, FIX 17) — the one-time human Accept is the honest path.
  local app_running=0 settings_exists=0 hint
  if pgrep -f "/Applications/Docker.app" >/dev/null 2>&1; then app_running=1; fi
  if [ -f "$HOME/Library/Group Containers/group.com.docker/settings-store.json" ] ||
    [ -f "$HOME/Library/Group Containers/group.com.docker/settings.json" ]; then
    settings_exists=1
  fi
  hint=$(docker_start_failure_hint "$app_running" "$settings_exists")
  case "$hint" in
  first_launch)
    tg "🆘 Docker Desktop is open but has never finished its FIRST-TIME setup, so its engine cannot start. One-time fix: click the Docker window, press Accept on the service agreement, finish the setup screens (skipping sign-in is fine), wait for the whale icon to settle, then double-click Start TickVault again."
    ;;
  app_stuck)
    tg "🆘 Docker Desktop is open but its engine did not answer within 120s. Click the Docker window — if it shows an update or a dialog, let it finish — then double-click Start TickVault again."
    ;;
  *)
    tg "🆘 Docker daemon did not come up within 120s — start Docker Desktop manually, then double-click Start TickVault."
    ;;
  esac
  return 1
}

# Live container state string for tv-questdb (empty when no container).
# FIX 18 G1: bounded via dk — `docker inspect` on a half-dead socket blocked
# forever inside questdb_session_healthy's per-minute probe.
questdb_container_state() {
  dk inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' tv-questdb 2>/dev/null || true
}

# FIX 16 (F10/F11): per-OUTAGE-EPISODE latches. The monitor loop re-invokes
# ensure_questdb every 60s while the database is unhealthy; before this fix
# every invocation re-initialised its restart budget (a fresh `docker
# restart` per cycle, unbounded during a crash loop) and re-sent its own
# unlatched ⚠️/🆘 Telegrams (~one page per 1-3 minutes for the rest of the
# session — the qdb_warned latch covered only the LOOP's message, not
# these). One episode = one restart + one page per message class; all
# latches re-arm when the database answers again (next episode pages anew).
QDB_EPISODE_RESTARTED=0
QDB_EPISODE_PAGED=0
QDB_FOREIGN_PAGED=0

ensure_questdb() {
  export_compose_creds # FIX 5.3 — compose interpolates TV_QUESTDB_PG_* env
  # FIX 18 G13: a fresh Docker has no database image — `up -d` folds the
  # pull in, and a Docker Hub rate-limit / offline network was then
  # misdiagnosed as "Docker not running" (the dev-path setup script had the
  # network diagnosis; the two-button path never reached it). Pull
  # explicitly FIRST (only when the image is absent) and classify a pull
  # failure by its own output.
  local qdb_image
  qdb_image=$(awk '/^  tv-questdb:/{f=1} f && /image:/{print $2; exit}' deploy/docker/docker-compose.yml 2>/dev/null || true)
  if [ -n "$qdb_image" ] && ! dk image inspect "$qdb_image" >/dev/null 2>&1; then
    log "database image ($qdb_image) is not on this Mac yet — downloading it first (one-time on a fresh Docker; bounded ${DOCKER_LONG_TIMEOUT_SECS}s)"
    local pull_out pull_class
    pull_out=$(with_timeout "$DOCKER_LONG_TIMEOUT_SECS" docker pull "$qdb_image" 2>&1) || true
    printf '%s\n' "$pull_out" >>"$(current_log)" 2>/dev/null || true
    if ! dk image inspect "$qdb_image" >/dev/null 2>&1; then
      pull_class=$(pull_failure_class "$pull_out")
      case "$pull_class" in
      rate_limit)
        tg "🆘 The database image download was BLOCKED by Docker Hub's download limit (shared home internet hits this). Wait about an hour and double-click Start TickVault again — or run 'docker login' once with a free Docker account to lift the limit."
        return 1
        ;;
      offline)
        tg "🆘 The database image could not be downloaded — this Mac looks OFFLINE (or the Wi-Fi needs a login page first). Get the internet working, then double-click Start TickVault."
        return 1
        ;;
      *)
        log "database image pull did not complete (unclassified) — letting compose retry the pull as part of start-up"
        ;;
      esac
    fi
  fi
  # B6: a compose failure must not be swallowed — log it loudly and keep
  # probing (the container may already be running from an earlier start),
  # but the timeout message will name the compose failure as the lead cause.
  # FIX 18 G1: bounded via dk --long (a wedged daemon must not hang boot).
  local compose_failed=0
  if ! dk --long compose -f deploy/docker/docker-compose.yml \
    -f deploy/docker/docker-compose.local.yml up -d tv-questdb >>"$(current_log)" 2>&1; then
    compose_failed=1
    log "docker compose up for the database FAILED (details above in this log) — still probing in case the container is already running"
  fi
  # Self-heal (2026-07-04): a degraded container (unhealthy/exited/dead/
  # paused — e.g. after a hard Mac sleep or an out-of-memory kill) gets ONE
  # automatic `docker restart` PER OUTAGE EPISODE if the database has not
  # answered after ~30s. Zero operator action — Stop→Start (or just Start)
  # recovers it.
  local i state verdict
  for i in $(seq 1 24); do
    if curl -sf -m 5 -G "$QUESTDB_HTTP/exec" --data-urlencode "query=select 1" >/dev/null 2>&1; then
      # B6: verify the responder IS our tv-questdb container — any local
      # process could hold port 9000 (a foreign QuestDB, a dev server).
      # Writing the day's capture into a stranger's database is silent loss.
      verdict="$(questdb_owner_verdict "$(questdb_container_state)")"
      if [ "$verdict" != "ok" ]; then
        log "port answers but the tv-questdb container is '$verdict' — a foreign program is on the database port"
        # F11: this condition is unrecoverable in-loop (compose can never
        # bind the taken port) — page ONCE per episode, then log-only.
        if [ "$QDB_FOREIGN_PAGED" = "0" ]; then
          QDB_FOREIGN_PAGED=1
          tg "🆘 Something else is answering on the database port and it is NOT our database container. Quit that other program (check Docker Desktop / anything using port 9000), then double-click Start TickVault."
        fi
        return 1
      fi
      log "QuestDB HTTP ready (tv-questdb container confirmed running)"
      QDB_EPISODE_RESTARTED=0 # healthy again — re-arm the episode latches
      QDB_EPISODE_PAGED=0
      QDB_FOREIGN_PAGED=0
      return 0
    fi
    state="$(questdb_container_state)"
    if ((i >= 6)) && questdb_state_degraded "$state"; then
      if [ "$QDB_EPISODE_RESTARTED" = "0" ]; then
        QDB_EPISODE_RESTARTED=1
        # FIX 18 G16: an OOM-kill by the container's OWN memory limit must
        # be named as OOM — a generic "was stuck" hid the real cause
        # (Docker VM too small / clamped limit) behind a restart that would
        # just OOM again under the same load.
        local oom
        oom=$(dk inspect --format '{{.State.OOMKilled}}' tv-questdb 2>/dev/null || true)
        if oomkilled_is_true "$oom"; then
          log "QuestDB container degraded ('$state') — OOM-KILLED by its own memory limit (QDB_MEM_LIMIT=$QDB_MEM_LIMIT); one automatic docker restart"
          tg "⚠️ The database ran OUT OF ITS MEMORY ALLOWANCE and was stopped by Docker — restarting it automatically. If this repeats today, open Docker Desktop → Settings → Resources → Memory, raise it to 10 GB, click Apply & Restart."
        else
          log "QuestDB container degraded ('$state') — one automatic docker restart"
          tg "⚠️ The database container was stuck — restarting it automatically. No action needed."
        fi
        dk --long restart tv-questdb >>"$(current_log)" 2>&1 || true
      elif ((i == 6)); then
        log "QuestDB container still degraded ('$state') — the one automatic restart for this outage was already used; waiting"
      fi
    fi
    sleep 5
  done
  if [ "$QDB_EPISODE_PAGED" = "0" ]; then
    QDB_EPISODE_PAGED=1
    if ((compose_failed)); then
      tg "🆘 Docker could not bring the database container up (the compose command itself failed) and the database never answered. Open Docker Desktop, make sure it is running, then double-click Start TickVault."
    else
      tg "🆘 The database did not answer within 2 minutes. It will keep being retried automatically every minute; if this started mid-session, no clicks are needed unless the next message says so. If it happened at startup: double-click Stop TickVault, wait 10 seconds, then double-click Start TickVault."
    fi
  else
    log "database still not answering after a full heal attempt — already paged this outage; retrying on the next monitor cycle"
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
  # FIX 18 G8/G14: keep-awake FLOOR — a Start after 15:45 IST previously
  # armed ZERO caffeinate (secs_until returned 0, no log line either), so
  # the evening cold release build / weekend probe ladder ran with no
  # idle-sleep protection on battery. Now max(until 15:45, the floor); the
  # 15:35 EOD stop still disarms it early on trading days. caffeinate -dims
  # cannot prevent lid-CLOSE sleep — the lid must stay OPEN regardless.
  secs=$(caffeinate_secs "$CAFFEINATE_UNTIL_IST" "$(ist_hms)" "$CAFFEINATE_MIN_SECS")
  if ((secs > 0)); then
    # FIX 16 (F5): own process group (mirror of the FIX 15.4 app launch).
    # A plain `caffeinate &` lived in the launcher shell's process group;
    # cmd_dev then `exec tail -f`s in that SAME group, so the IDE Stop
    # button (group signal) killed caffeinate with the tail while the app
    # survived — the Mac silently lost sleep protection for the day, and
    # the 08:55 robot even skipped re-arming while the doomed dev
    # caffeinate still held the pidfile. The own-group launch detaches it.
    (
      set -m
      nohup caffeinate -dims -t "$secs" >/dev/null 2>&1 &
      echo $! >"$CAFF_PIDFILE"
    )
    log "caffeinate armed for ${secs}s (target $CAFFEINATE_UNTIL_IST IST or the $((CAFFEINATE_MIN_SECS / 3600))h floor, whichever is later; lid must stay OPEN)"
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

# FIX 19 item 7: read the app's public per-feed health (GET /api/feeds/health)
# and print one "name|enabled|verdict|connected|subscribed" line per feed
# (lowercased). Empty output on any failure — the caller keeps polling.
feed_health_rows() {
  curl -sf -m 5 "http://127.0.0.1:${APP_API_PORT}/api/feeds/health" 2>/dev/null |
    python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for f in d.get("feeds", []):
    print("|".join(str(f.get(k, "")) for k in ("feed", "enabled", "verdict", "connected", "subscribed_total", "ticks_total")).lower())
' 2>/dev/null || true
}

# FIX 19 item 7: GUARANTEED per-feed Telegram status after a launch — one
# message per feed, EVERY mode (an OFF feed is announced too; silence was
# the bug). Polls the app's public feeds-health endpoint for up to
# <wait_secs> (default 180) so the subscribe counts have time to land, then
# sends whatever truth is available. Runs as the detached `feed-pings`
# subcommand so no start path blocks on it.
send_feed_status_pings() {
  local wait_secs="${1:-180}" waited=0 rows=""
  case "$wait_secs" in '' | *[!0-9]*) wait_secs=180 ;; esac
  while [ "$waited" -lt "$wait_secs" ]; do
    rows=$(feed_health_rows)
    if feed_rows_settled "$rows"; then break; fi
    sleep 5
    waited=$((waited + 5))
  done
  if [ -z "$rows" ]; then
    tg "⚠️ Feed status check: the app did not answer its status endpoint within ${wait_secs}s, so the per-feed states are unknown. It may still be booting — check the next messages or the log."
    return 0
  fi
  # FIX 20 task 2: compute the market-open flag ONCE so the wording is
  # state-based (ticks>0 = flowing; 0 ticks + closed = awaiting; 0 ticks +
  # open = honest warning).
  local mkt=0
  if in_market_hours_ist "$(hhmm_to_secs "$(ist_hms)")" "$(ist_weekday)"; then mkt=1; fi
  local name enabled verdict connected sub ticks
  while IFS='|' read -r name enabled verdict connected sub ticks; do
    [ -n "$name" ] || continue
    tg "$(feed_ping_text "$name" "$enabled" "$verdict" "$connected" "$sub" "$ticks" "$mkt")"
  done <<<"$rows"
  return 0
}

# FIX 19 item 7: detach the feed-status pinger so no start path blocks on
# the up-to-3-minute poll (mirrors the FIX 18 G5 monitor spawn pattern —
# own process group, survives the parent's exit).
spawn_feed_status_pings() {
  (
    set -m
    nohup bash "$0" feed-pings >>"$(current_log)" 2>&1 &
  ) || true
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
    # FIX 16 (F2): rc 3 (NOT 0) — this launcher did NOT start anything and
    # has no idea how the other launcher's start will end. Returning 0 here
    # let cmd_start fire its unconditional "🚀 Manual start complete — app
    # running" Telegram with no app running (charter Rule 11 false-OK).
    log "another launcher still owns the start after 60s — skipping this launch to avoid a double-start (the other launcher is handling it)"
    return 3
  fi
  local _rc=0
  start_app_inner "$@" || _rc=$?
  launcher_lock_release
  return $_rc
}

# start_app_tolerate_peer <label> — FIX 16 (F2): maps start_app's rc 3
# (another launcher owns the start — the app launch is in the PEER's hands)
# to success for flows that only need "a start is underway" (the robot's
# day flow + the monitor relaunch); every other rc passes through. The
# manual cmd_start does NOT use this — it must word its Telegram honestly.
start_app_tolerate_peer() {
  local rc=0
  start_app "$@" || rc=$?
  if [ "$rc" = "3" ]; then
    log "another launcher is starting the app — proceeding without a second start (the monitor will adopt it once up)"
    return 0
  fi
  return $rc
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
  # FIX 15.3: honor a Stop clicked DURING the (possibly minutes-long) build —
  # previously the launch below silently overrode a marker written mid-build.
  if manual_stop_active "$MANUAL_STOP_MARKER" "$(ist_date)"; then
    log "manual stop marker appeared during the build — NOT launching the app (Stop always wins)"
    tg "🔔 Stop was pressed while the app was still building — honoring it: the app was NOT started. Double-click Start TickVault whenever you are ready."
    return 1
  fi
  # FIX 15.1: raise the open-files soft limit before launch (inherited by
  # the app) — the 100-connection fleet exhausts the macOS default 256.
  raise_fd_limit
  # FIX 15.4: launch in its OWN process group (`set -m` gives background
  # jobs their own pgid — the portable equivalent of setsid, which macOS
  # has no binary for) so a Ctrl-C / IDE-Stop in the tail window can never
  # SIGINT the app; nohup keeps the SIGHUP immunity.
  (
    set -m
    nohup ./target/release/tickvault >>"$(current_log)" 2>&1 &
    echo $! >"$APP_PIDFILE"
  )
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
# hint_docker_vm_memory <vm_bytes> <limit_bytes> — FIX 17. Replaces the
# FIX-7/14 auto-raise, which QUIT + RELAUNCHED Docker Desktop mid-run:
# proven live 2026-07-05, that (a) killed the QuestDB container the launcher
# had just started and hung the whole run waiting for a fresh Docker
# Desktop, and (b) on Docker Desktop 4.80 the settings-store.json MemoryMiB
# edit was IGNORED anyway (Settings → Resources still showed 8 GB after the
# restart), so every subsequent launch saw VM < target and re-fired the
# raise + restart — an infinite restart loop. Investigated lever: the
# Docker Desktop 4.x `docker desktop` admin CLI carries only
# start/stop/restart/status/update/module subcommands — there is NO
# memory/resource setter, so NO reliable non-restarting programmatic path
# exists. The HONEST behavior is therefore:
#   PRIMARY  — the caller clamps QDB_MEM_LIMIT to (VM − 1 GB) and PROCEEDS
#              (QuestDB is healthy at 4g per aws-budget; 8 GB Docker works).
#   OPTIONAL — the operator may raise the slider ONCE in Docker Desktop →
#              Settings → Resources → Memory (clearly optional).
#   HINT     — this function writes the preferred value into the settings
#              file at most ONCE as a best-effort hint for the next natural
#              Docker start. It NEVER quits/launches/restarts Docker, NEVER
#              waits on the daemon, NEVER blocks the run, and NEVER claims
#              the raise worked. Idempotent via docker_mem_hint_decision
#              (skip when the file already carries >= target — no rewrite
#              loop even though 4.80 ignores the file).
# Always returns 1 so the caller ALWAYS takes the clamp-and-proceed path.
hint_docker_vm_memory() {
  [ "$(uname -s)" = "Darwin" ] || return 1
  [ -z "${CI:-}" ] || return 1
  local vm_mib=$(($1 / 1048576))
  # Required = QuestDB pin + 2 GB headroom (Docker itself + the app's containers).
  local required_mib=$(($2 / 1048576 + 2048))
  local host_bytes host_mib
  host_bytes=$(sysctl -n hw.memsize 2>/dev/null || true)
  case "$host_bytes" in '' | *[!0-9]*) return 1 ;; esac
  host_mib=$((host_bytes / 1048576))
  # FIX 19 addendum: the default target is host-aware — 12 GB on a >= 32 GB
  # Mac (the operator's is 48 GB), 10 GB otherwise. TV_DOCKER_VM_MEM_MIB
  # still overrides both paths (hint here + cold-start raise).
  local pref="${TV_DOCKER_VM_MEM_MIB:-}"
  case "$pref" in '' | *[!0-9]*) pref=$(docker_mem_target_mib "$host_bytes") ;; esac
  local target
  if ! target=$(docker_vm_raise_target "$vm_mib" "$required_mib" "$pref" "$host_mib"); then
    return 1
  fi
  case "$target" in ok | host_small) return 1 ;; esac
  # Locate the settings file + its memory key (Docker Desktop ≥ 4.30 store
  # first, then the legacy file). FIX 14 insert path (rc 10) kept.
  local gc="$HOME/Library/Group Containers/group.com.docker"
  local file="" key="" cand rc
  for cand in "$gc/settings-store.json" "$gc/settings.json"; do
    # errexit-safe capture: the || arm keeps a nonzero rc from aborting.
    key=$(docker_settings_mem_key "$cand") && rc=0 || rc=$?
    case "$rc" in
    0 | 10)
      file="$cand"
      break
      ;;
    *) : ;; # absent / bad JSON / python3 failure — try the next candidate
    esac
  done
  [ -n "$file" ] || return 1
  local current decision
  current=$(docker_settings_mem_get "$file" "$key" 2>/dev/null || true)
  decision=$(docker_mem_hint_decision "$current" "$target")
  if [ "$decision" = "skip" ]; then
    log "Docker memory preference already recorded (${current:-n/a} MiB in $(basename "$file")) — not rewriting; it applies whenever Docker Desktop is next restarted"
    return 1
  fi
  if docker_settings_set_mem "$file" "$key" "$target"; then
    log "wrote Docker memory preference ($((target / 1024)) GB) — it applies the next time you start Docker; continuing now at current memory (${vm_mib} MiB)"
  else
    log "could not write the Docker memory preference file — continuing at current memory (${vm_mib} MiB)"
  fi
  return 1
}

# check_docker_vm_memory — warn (log + Telegram, non-blocking) when the
# Docker VM has less memory than the QuestDB pin (QDB_MEM_LIMIT). On Docker
# Desktop the VM ceiling silently caps the container limit — QuestDB would
# be OOM-killed under load instead of using its pinned budget.
check_docker_vm_memory() {
  local vm_bytes limit_bytes
  vm_bytes=$(dk info --format '{{.MemTotal}}' 2>/dev/null || true)
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
    # FIX 17: NEVER restart Docker mid-run (the FIX-7 auto-raise quit +
    # relaunched Docker Desktop — killing the just-started database
    # container, hanging the launch, and re-firing every run because
    # Docker Desktop 4.80 ignored the settings-file edit). The clamp below
    # is the PRIMARY path; the hint write is best-effort, at-most-once,
    # and never blocks or restarts anything.
    hint_docker_vm_memory "$vm_bytes" "$limit_bytes" || true
    # FIX 5.4: do NOT launch a database container doomed to be killed for
    # memory — clamp the limit for THIS RUN to (VM memory − 1 GB, min 1g).
    local clamped
    if clamped=$(clamp_qdb_mem_g "$vm_bytes"); then
      log "auto-clamping QDB_MEM_LIMIT ${QDB_MEM_LIMIT} → ${clamped} for this run"
      # FIX 18 G16: the clamp silently defeated the 8g max-ladder pin while
      # telling the operator it was fine — honest wording now, and
      # the clamped flag makes the scale branch SKIP the 100k max ladder
      # (an under-provisioned database's OOM pressure would be misread as
      # a Groww per-account cap).
      QDB_MEM_CLAMPED=1
      tg "ℹ️ Docker Desktop has ${vm_gb} GB memory, so the database will use ${clamped} this run — fine for a NORMAL capture day. On a max-scale (100,000-instrument) day the full ladder needs more: open Docker Desktop → Settings → Resources → Memory, raise it to 10 GB, click Apply & Restart (one time)."
      export QDB_MEM_LIMIT="$clamped"
    else
      QDB_MEM_CLAMPED=1 # FIX 18 G16: VM smaller than the pin — max ladder unsafe here too
      tg "⚠️ Docker Desktop only has ${vm_gb} GB memory and the database's ${QDB_MEM_LIMIT} budget could not be adjusted automatically. Running anyway. Optional fix: open Docker Desktop → Settings → Resources → Memory and raise it to 10 GB, then Apply & Restart."
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
  curl -sf -m 5 -G "$QUESTDB_HTTP/exec" --data-urlencode \
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
  # FIX 18 G15: pre-build OUTSIDE the probe's hard time cap. A fresh
  # clone's cold `cargo build --release` (codegen-units=1 + lto) previously
  # burned most of the 45-minute cap and the truncated ladder was
  # misrecorded as "Groww capped us at N" — corrupting the experiment. A
  # failed build skips the probe WITHOUT burning the attempt.
  log "pre-building the release binary before the probe (a cold build can take many minutes — it does NOT count against the probe's time cap)"
  if ! cargo build --release >>"$(current_log)" 2>&1; then
    tg "🆘 Build FAILED — the one-time 100-connection lab test cannot run today (the attempt was NOT used up). Last lines: $(tail -5 "$(current_log)" | tr '\n' ' | ')"
    return 0
  fi
  mkdir -p data/groww-scale
  # Stamp BEFORE the probe: even a crash mid-probe must not re-trigger it —
  # the very next click goes straight to the normal start.
  printf '%s %s probe-once attempt %s\n' "$today" "$(ist_hms)" "$attempt" >"$stamp"
  local probe_start_epoch rows_before rows_after
  probe_start_epoch=$(date +%s)
  # FIX 15.6: snapshot the ladder-evidence row count so a FAILED-TO-LAUNCH
  # probe (zero new rows) can be told apart from a partial run.
  rows_before=$(tsv_rows "data/groww-scale/summary-${today}.tsv")
  export_compose_creds
  # FIX 16 (F6/F7): the wrapper now exits with DISTINCT codes — before this,
  # it exited 0 on virtually every failure in auto-stop mode (the FIX 15.6
  # un-burn was dead code for the real launch-failure class), while a
  # Ctrl-C during the pre-first-row window DELETED the stamp and invited a
  # second full Groww fleet storm the same day.
  local probe_rc=0
  # FIX 19 item 9: per-feed Telegram during the probe window too — the 3
  # normal start paths already spawn the pinger (item 7), but the probe app
  # serves the same feeds-health endpoint and the operator was blind here.
  # Detached; polls up to 180s while the probe app boots.
  spawn_feed_status_pings # FIX 19 item 9: probe-window per-feed Telegram
  # FIX 19 item 9: cap default raised 45 -> 75 min. With the 30s smoke
  # warm-up, 8 rungs x ~4.5 min + ~10 min setup = ~46-50 min — the old 45
  # cap truncated the ladder around rung 80 and burned the attempt with a
  # FALSE "Groww capped us" verdict (FIX 18 G15 class).
  PROBE_AUTO_STOP_MIN="${PROBE_ONCE_MAX_MIN:-75}" bash scripts/scale-100-probe.sh || probe_rc=$?
  case "$(probe_rc_class "$probe_rc")" in
  complete)
    # Belt-and-braces (F6): a clean exit must still show evidence rows —
    # zero rows on rc 0 is a launch failure whatever the wrapper thinks.
    rows_after=$(tsv_rows "data/groww-scale/summary-${today}.tsv")
    if probe_launch_failed "$rows_before" "$rows_after"; then
      rm -f "$stamp"
      log "probe exited clean but wrote ZERO ladder evidence rows — treating as failed-to-launch; attempt stamp deleted"
      tg "🆘 The one-time 100-connection lab test could not even start (no evidence was written). The attempt was NOT used up — double-click Start TickVault to try again. Continuing a NORMAL session now."
    else
      log "probe finished — continuing into normal live start"
    fi
    ;;
  interrupted)
    # F7: interrupted mid-run — Groww connections may ALREADY have been
    # opened, so the one-shot attempt MUST stay burned (the whole point of
    # the stamp is never storming Groww twice in a day).
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
    log "probe was interrupted (rc ${probe_rc}) — attempt stays used for today (Groww connections may already have been opened)"
    tg "⚠️ The one-time 100-connection lab test was stopped before it finished. The attempt stays used for today — connections may already have been opened, and the test must never run twice in one day. Continuing a NORMAL session now."
    ;;
  never_launched)
    # F6: the wrapper verified ZERO evidence rows and NO signal — the probe
    # never launched (build failure, docker failure, in-app preflight FAIL
    # falling back to single-conn). Safe to un-burn.
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
    rm -f "$stamp"
    log "probe FAILED TO LAUNCH (zero new ladder evidence rows) — attempt stamp deleted; the next Start click will retry the probe automatically"
    tg "🆘 The one-time 100-connection lab test could not even start (no evidence was written). The attempt was NOT used up — double-click Start TickVault to try again. Continuing a NORMAL session now."
    ;;
  *)
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
    rows_after=$(tsv_rows "data/groww-scale/summary-${today}.tsv")
    if probe_launch_failed "$rows_before" "$rows_after"; then
      # Unknown failure with zero evidence — same never-launched class.
      rm -f "$stamp"
      log "probe FAILED TO LAUNCH (rc ${probe_rc}, zero new ladder evidence rows) — attempt stamp deleted; the next Start click will retry the probe automatically"
      tg "🆘 The one-time 100-connection lab test could not even start (no evidence was written). The attempt was NOT used up — double-click Start TickVault to try again. Continuing a NORMAL session now."
    else
      log "probe did not complete cleanly (rc ${probe_rc}) — overlay cleaned; continuing into normal live start anyway"
      tg "⚠️ The one-time 100-connection lab test stopped partway. The evidence rows it DID write are saved for the verdict; continuing a NORMAL session now. Details are in the log."
    fi
    ;;
  esac
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
  # FIX 15.1: raise the open-files limit at entry so the 100-connection
  # probe (launched from THIS shell) inherits it too, not just the app.
  raise_fd_limit
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
  # FIX 16 (F2): rc 3 = the OTHER launcher (e.g. the 08:55 robot mid-build)
  # owns the start — say so honestly instead of the false "start complete".
  local start_rc=0
  start_app "manual start" || start_rc=$?
  if [ "$start_rc" = "3" ]; then
    tg "🔔 Another launcher is already starting the app (probably the scheduled 8:55 AM robot) — this click will not double-start. Watch this window; the app comes up when that start finishes."
    return 0
  fi
  [ "$start_rc" = "0" ] || return 1
  tg "🚀 Manual start complete — app running, feed capture live. Autopilot (if scheduled) will adopt this app, not double-start."
  spawn_feed_status_pings # FIX 19 item 7: one Telegram per feed, every mode
  # FIX 18 G5: a manual Start with no 08:55 robot alive previously ran the
  # whole day with NO monitor — no 15:35 EOD stop (the app + broker session
  # ran on for hours after close), no one-shot crash relaunch, no
  # mid-session disk/database self-heal. Spawn the detached monitor
  # subcommand when nothing else is covering the day.
  maybe_spawn_manual_monitor
}

# maybe_spawn_manual_monitor — G5 runtime half of manual_monitor_needed:
# spawn the detached `monitor` subcommand unless (a) a cmd_run instance is
# alive (it owns the day), (b) a monitor is already running, or (c) the EOD
# stop has already passed (an evening session runs until Stop is pressed).
maybe_spawn_manual_monitor() {
  local run_alive=0 mon_alive=0 pid
  pid=$(awk '{print $1}' "$LOCKDIR/pid" 2>/dev/null || true)
  case "$pid" in
  '' | *[!0-9]*) : ;;
  *) if kill -0 "$pid" 2>/dev/null; then run_alive=1; fi ;;
  esac
  pid=$(cat "$MONITOR_PIDFILE" 2>/dev/null || true)
  case "$pid" in
  '' | *[!0-9]*) : ;;
  *) if kill -0 "$pid" 2>/dev/null; then mon_alive=1; fi ;;
  esac
  if [ "$(manual_monitor_needed "$run_alive" "$mon_alive" "$(secs_until "$EOD_STOP_IST" "$(ist_hms)")")" = "spawn" ]; then
    (
      set -m
      nohup bash "$0" monitor >>"$(current_log)" 2>&1 &
      echo $! >"$MONITOR_PIDFILE"
    )
    log "background monitor spawned (pid $(cat "$MONITOR_PIDFILE" 2>/dev/null || echo '?')) — EOD stop $EOD_STOP_IST, crash relaunch and database self-heal now cover this manual start"
  else
    log "no separate monitor spawned (scheduled run active, monitor already running, or past $EOD_STOP_IST IST — an evening session runs until you press Stop)"
  fi
}

# ── Subcommand: monitor (FIX 18 G5 — safety net behind a manual Start) ─────
# Reuses the EXACT monitor loop + EOD tail the 08:55 robot uses; detached by
# maybe_spawn_manual_monitor. A later cmd_run takes the day over by stopping
# this monitor (it reads MONITOR_PIDFILE at startup).
cmd_monitor() {
  echo $$ >"$MONITOR_PIDFILE"
  log "manual-start monitor engaged (pid $$) — EOD stop $EOD_STOP_IST IST, crash relaunch + database self-heal active"
  monitor_until_eod
  if ! manual_stop_active "$MANUAL_STOP_MARKER" "$(ist_date)"; then
    stop_app
    bash scripts/groww-scale-test.sh clean >>"$(current_log)" 2>&1 || true
  fi
  stop_caffeinate
  eod_summary "manual"
  rm -f "$MONITOR_PIDFILE" 2>/dev/null || true
  log "manual-start monitor complete"
}

# stop_manual_monitor — kill the G5 background monitor (Stop always wins;
# cmd_run also calls this when it takes ownership of the day).
stop_manual_monitor() {
  local pid
  pid=$(cat "$MONITOR_PIDFILE" 2>/dev/null || true)
  case "$pid" in
  '' | *[!0-9]*) : ;;
  *)
    if kill -0 "$pid" 2>/dev/null; then
      log "stopping the background manual-start monitor (pid $pid)"
      kill "$pid" 2>/dev/null || true
    fi
    ;;
  esac
  rm -f "$MONITOR_PIDFILE" 2>/dev/null || true
}

# ── Subcommand: manual stop ─────────────────────────────────────────────────

cmd_stop() {
  # FIX 19 item 3: when the probe wrapper invokes this stop as its own
  # runway-clearing step (AUTOPILOT_PROBE_PREP=1 exported by the wrapper),
  # the operator just clicked Run — the message must say the live start
  # FOLLOWS automatically, not that autopilot stands down. Behavior is
  # otherwise identical (the marker is still written; the probe path
  # tolerates it via its manual_stop_during_probe grace window).
  local notice_mode
  notice_mode="$(stop_notice_mode "${AUTOPILOT_PROBE_PREP:-0}")"
  if [ "$notice_mode" = "probe_prep" ]; then
    log "== PROBE-PREP STOP (clearing the runway for the one-time lab test — part of the same Run click) =="
  else
    log "== MANUAL STOP (writing manual-stop marker — autopilot stands down today) =="
  fi
  stop_manual_monitor # FIX 18 G5: Stop always wins over the background monitor
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
      dk --long restart tv-questdb >>"$(current_log)" 2>&1 || true
    fi
  fi
  printf '%s %s manual stop\n' "$TODAY" "$(ist_hms)" >"$MANUAL_STOP_MARKER"
  if [ "$notice_mode" = "probe_prep" ]; then
    tg "🔬 probe preparing: clearing the runway (this is part of your Run click — the live start follows automatically)"
  else
    tg "🔔 Manual stop done — app stopped, overlay cleaned, QuestDB left running. Autopilot will NOT auto-restart today. Double-click Start TickVault to resume."
  fi
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
  # FIX 15.5: the IntelliJ Run button is how the operator actually clicks —
  # give it the SAME self-update Start TickVault.command has (fetch +
  # ff-only merge, graceful fallback; skipped off-branch). After a REAL
  # update, re-exec the fresh script once (guard env stops a loop) so the
  # new code — including a bumped probe marker — actually runs.
  if [ "${TICKVAULT_DEV_SELF_UPDATED:-0}" != "1" ]; then
    local dev_branch head_before head_after
    dev_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
    if [ "$dev_branch" = "local-runtime" ]; then
      # FIX 16 (F1): the ff-merge rewrites the tree — acquire-or-skip the
      # launcher lock so it can never run while the 08:55 robot's lock-held
      # `cargo build --release` is reading the sources (Monday: robot
      # fetches 08:55, builds for minutes; a Run click at 08:57 must not
      # merge fresh commits into crates/*.rs under rustc). F3: the merge
      # gets one retry so a transient .git/index.lock loser is not
      # misdiagnosed as local changes.
      if ! launcher_lock_acquire_wait 5; then
        log "self-update skipped — another launcher is mid-flight (likely building); running the existing code so the tree cannot change under its build"
      else
        head_before=$(git rev-parse HEAD 2>/dev/null || echo "")
        # FIX 18 G13: fetch is bounded — a captive/black-hole Wi-Fi made it
        # HANG (a hang is not a failure; the offline fallback never fired).
        if with_timeout "$GIT_NET_TIMEOUT_SECS" git fetch origin local-runtime >>"$(current_log)" 2>&1; then
          if git_ff_merge_with_retry origin/local-runtime; then
            head_after=$(git rev-parse HEAD 2>/dev/null || echo "")
            if [ -n "$head_before" ] && [ "$head_before" != "$head_after" ]; then
              log "self-update applied ($head_before -> $head_after) — restarting the launcher on the new code"
              launcher_lock_release # MUST release before exec — same pid would deadlock itself in start_app
              TICKVAULT_DEV_SELF_UPDATED=1 exec bash "$0" dev
            fi
            log "self-update: code already up to date"
          else
            log "self-update: could not fast-forward (local changes in the way?) — running the EXISTING code"
            tg "⚠️ The Run button could not apply the latest update — started with the code already on the Mac. Everything still runs."
          fi
        else
          log "self-update: git fetch failed (offline?) — running the existing code"
        fi
        launcher_lock_release
      fi
    else
      log "self-update: not on the local-runtime branch ($dev_branch) — skipping"
    fi
  fi
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

# git_ff_merge_with_retry <ref> — FIX 16 (F3): one short retry absorbs a
# transient .git/index.lock loser (two launchers merging in the same second
# at 08:55 — the loser's error is indistinguishable from a dirty tree in
# git's output, so without the retry it was misdiagnosed as "local changes
# in the way" and paged the operator for nothing).
git_ff_merge_with_retry() {
  if git merge --ff-only "$1" >>"$(current_log)" 2>&1; then return 0; fi
  sleep 1
  git merge --ff-only "$1" >>"$(current_log)" 2>&1
}

preflight_git() {
  # FIX 16 (F1): the merge below REWRITES the working tree — it must never
  # run while another launcher's lock-held `cargo build --release` is
  # reading the sources (rustc mid-build over a changing tree = compile
  # failure or a mixed old/new binary the adopt gate cannot detect).
  # Acquire-or-skip: a held lock means a build/start is in flight — run
  # the existing code this round rather than mutate under it.
  if ! launcher_lock_acquire_wait 5; then
    log "another launcher is mid-flight — skipping the git code update this run (the tree must not change under an in-flight build)"
    return 0
  fi
  local branch
  branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
  if [ "$branch" != "local-runtime" ]; then
    log "on branch '$branch' — attempting checkout of local-runtime"
    if ! git checkout local-runtime >>"$(current_log)" 2>&1; then
      tg "⚠️ Could not switch to the local-runtime branch (uncommitted changes?). Running the EXISTING checkout '$branch' — code may be stale."
      launcher_lock_release
      return 0
    fi
  fi
  # FIX 18 G13: fetch is bounded — a captive/black-hole Wi-Fi at 08:55 made
  # it HANG up to the OS TCP timeout instead of taking the offline fallback.
  if with_timeout "$GIT_NET_TIMEOUT_SECS" git fetch origin >>"$(current_log)" 2>&1; then
    if ! git_ff_merge_with_retry origin/local-runtime; then
      tg "⚠️ local-runtime has diverged from origin (conflict?) — running the EXISTING local code. Resolve with: git merge origin/local-runtime"
    fi
  else
    log "git fetch failed or timed out (offline / captive Wi-Fi?) — running existing code"
  fi
  launcher_lock_release
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
  local qdb_warned=0 vm_disk_warned=0 vm_pct qdb_fail_streak=0 healthy_streak=0
  VM_DISK_WARN_PCT="${VM_DISK_WARN_PCT:-85}"
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
      # FIX 15.8: the host df above cannot see Docker's own virtual disk
      # filling (QuestDB writes land INSIDE the VM). Probe the VM fs use%
      # through the database container; edge-latched warning >85%; a
      # `docker system df` breakdown goes to the log for context.
      vm_pct=$(docker_vm_disk_pct)
      if vm_disk_high "$vm_pct" "$VM_DISK_WARN_PCT"; then
        if [ "$vm_disk_warned" = "0" ]; then
          dk system df >>"$(current_log)" 2>&1 || true
          tg "⚠️ Docker's own virtual disk is ${vm_pct}% full (above ${VM_DISK_WARN_PCT}%). The database lives inside it — raise the Docker Desktop 'Virtual disk limit' (Settings → Resources) soon or the capture can stall. A space breakdown was written to the log."
          vm_disk_warned=1
        fi
        log "VM disk re-check: HIGH (${vm_pct}% > ${VM_DISK_WARN_PCT}%)"
      else
        if [ "$vm_disk_warned" = "1" ] && [ -n "$vm_pct" ]; then
          log "VM disk re-check: recovered (${vm_pct}%) — warn latch re-armed"
          vm_disk_warned=0
        fi
      fi
    fi
    # FIX 16 (F5): re-arm caffeinate each cycle — cheap no-op while the
    # pidfile holder is alive; restores sleep protection if caffeinate
    # died (IDE window closed, -t expiry drift) without any operator step.
    [ "$manual" = "0" ] && start_caffeinate
    decision=$(monitor_decision "$alive" "$manual" "$relaunches")
    case "$decision" in
    idle)
      # F4 re-arm: 30 healthy minutes after a death episode restores the
      # one-relaunch + one-alert budget (a NEW death later in the day pages
      # again), while a fast crash loop never re-arms (cap preserved).
      if ((relaunches > 0)); then
        healthy_streak=$((healthy_streak + 1))
        if ((healthy_streak >= 30)); then
          log "app healthy for 30 minutes — relaunch/alert budget re-armed"
          relaunches=0
          healthy_streak=0
        fi
      fi
      ;;
    idle_alerted)
      # FIX 16 (F4): the ONE alert already fired — stay quiet (one line per
      # cycle in the log, zero Telegram) instead of paging every 60s.
      log "app still down after the one relaunch + one alert — standing by (double-click Start TickVault to relaunch)"
      healthy_streak=0
      ;;
    manual_standdown)
      log "manual stop marker present — autopilot stands down (no relaunch, no fight)"
      ;;
    relaunch)
      relaunches=$((relaunches + 1))
      healthy_streak=0
      tg "⚠️ App died mid-session — relaunching once (attempt $relaunches). Log tail: $(tail -5 "$(current_log)" | tr '\n' ' | ')"
      sleep 30
      start_app_tolerate_peer "auto-relaunch" || true
      ;;
    alert)
      tg "🆘 App died AGAIN after the one relaunch — NOT retrying (avoid crash-looping the feed). Log tail: $(tail -8 "$(current_log)" | tr '\n' ' | '). Double-click Start TickVault after investigating."
      relaunches=$((relaunches + 1)) # FIX 16 F4: moves the state to idle_alerted — this alert fires ONCE
      healthy_streak=0
      ;;
    esac
    # Docker watchdog (independent of the app): one self-heal try + alert.
    if ! docker_alive && [ "$manual" = "0" ]; then
      tg "⚠️ Docker daemon went away mid-session — attempting restart"
      ensure_docker && ensure_questdb || true
    fi
    # FIX 15.2: the docker DAEMON can be alive while the tv-questdb
    # CONTAINER is dead (OOM-kill, crash after a hard sleep) — previously
    # undetected for the rest of the day (ticks spill, no alert). Cheap
    # per-iteration probe; ensure_questdb carries the restart self-heal.
    # Edge-latched Telegram: one dead episode = one message.
    if [ "$manual" = "0" ] && docker_alive; then
      if ! questdb_session_healthy; then
        qdb_fail_streak=$((qdb_fail_streak + 1))
        # FIX 16 (F12): the liveness probe is a single 3s curl sample — one
        # slow /exec response (ILP burst, GC pause, Mac wake blip) must not
        # page the operator or fire the heal. Require 2 CONSECUTIVE failed
        # samples (~2 minutes) before acting; the first miss is log-only.
        if ((qdb_fail_streak < 2)); then
          log "database re-check: one slow/failed sample — confirming on the next cycle before healing"
        else
          if [ "$qdb_warned" = "0" ]; then
            tg "⚠️ The database stopped answering mid-session — recovering it automatically now (the app buffers safely meanwhile)."
            qdb_warned=1
          fi
          log "database re-check: tv-questdb unhealthy (${qdb_fail_streak} consecutive samples) — running the ensure_questdb self-heal"
          ensure_questdb || true
        fi
      else
        if [ "$qdb_warned" = "1" ]; then
          log "database re-check: recovered — warn latch re-armed"
          qdb_warned=0
        fi
        qdb_fail_streak=0
      fi
    fi
  done
}

cmd_run() {
  # Duplicate-instance lock (mkdir is atomic; stale lock from a dead pid is
  # reclaimed). FIX 18 G1 compounding: the lock line now carries the DATE —
  # a WEDGED previous-day run (hung on a dead docker socket) previously
  # held the lock "alive" forever, so every next-morning fire exited
  # "another instance is running" and the day was silently lost. A live
  # holder from a DIFFERENT day is stopped and the lock taken over.
  if ! mkdir "$LOCKDIR" 2>/dev/null; then
    local old_line old old_date holder_alive=0 verdict
    old_line=$(cat "$LOCKDIR/pid" 2>/dev/null || echo "")
    old=$(printf '%s' "$old_line" | awk '{print $1}')
    old_date=$(printf '%s' "$old_line" | awk '{print $2}')
    if [ -n "$old" ] && kill -0 "$old" 2>/dev/null; then holder_alive=1; fi
    verdict=$(run_lock_takeover "$holder_alive" "$old_date" "$TODAY")
    case "$verdict" in
    exit)
      log "another autopilot instance (pid $old) is running — exiting"
      exit 0
      ;;
    steal)
      log "autopilot lock is held by a PREVIOUS day's run (pid $old, lock date ${old_date:-?}) — that run is stuck; stopping it and taking over"
      tg "⚠️ Yesterday's automation was found STUCK this morning — it was stopped automatically and today's run is starting normally."
      kill -TERM "$old" 2>/dev/null || true
      sleep 2
      if kill -0 "$old" 2>/dev/null; then kill -KILL "$old" 2>/dev/null || true; fi
      rm -rf "$LOCKDIR" 2>/dev/null || true
      if ! mkdir "$LOCKDIR" 2>/dev/null; then
        log "could not reclaim the autopilot lock after the takeover — exiting"
        exit 1
      fi
      ;;
    reclaim)
      log "reclaiming stale lock (pid ${old:-?} is gone)"
      ;;
    esac
  fi
  printf '%s %s\n' "$$" "$TODAY" >"$LOCKDIR/pid"
  trap 'rm -rf "$LOCKDIR"' EXIT
  # FIX 18 G5: the robot owns the day — stop any manual-start background
  # monitor so two monitor loops never fight over relaunch decisions.
  stop_manual_monitor

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

  # FIX 15.9 (belt-and-braces): warn if the AWS prod box is RUNNING while
  # this Mac boots Dhan-enabled — the dual-instance lock keeps it SAFE
  # (fail-closed) but the local day would be lost to a token fight.
  # Best-effort, never gates; the FIX 15.1 fd raise also covers the robot path.
  raise_fd_limit
  aws_prod_running_check

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
    start_app_tolerate_peer "scale day — probe overlay" || exit 1
    tg "🚀 Scale-lab day: app booted with the 2×600 cap-probe. Verdict expected between $PROBE_POLL_START_IST and ~10:05 IST — I will read it and act automatically."
    spawn_feed_status_pings # FIX 19 item 7: one Telegram per feed, every mode

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
      # FIX 18 G16: the 100k max ladder was pinned QDB_MEM_LIMIT=8g so it
      # gets its full database budget — on a clamped (small Docker VM) run
      # an OOM'd database would masquerade as a Groww cap. Skip the ladder,
      # keep the normal capture, tell the operator the one-time fix.
      if [ "$(scale_max_gate "${QDB_MEM_CLAMPED:-0}")" = "skip_max" ]; then
        tg "⚠️ PROBE VERDICT says the full 100K ladder could run — but Docker Desktop's memory is too small for the database at that scale, so the ladder is SKIPPED today (a normal capture continues). One-time fix: open Docker Desktop → Settings → Resources → Memory, raise it to 10 GB, click Apply & Restart — the next scale day will run the full ladder."
        stop_app
        apply_overlay none
        start_app_tolerate_peer "normal session (max ladder skipped: Docker memory clamped)" || true
      else
        tg "✅ PROBE VERDICT: both connections captured — the 1000 cap is PER-CONNECTION. Switching to the FULL 100K ladder now."
        stop_app
        apply_overlay max
        start_app_tolerate_peer "max-scale ladder" || exit 1
      fi
      ;;
    record_continue)
      tg "🔔 PROBE VERDICT: per-ACCOUNT limit — only one connection streams. That IS the experiment's answer; recorded in the audit table. Continuing a NORMAL session (no laddering)."
      stop_app
      apply_overlay none
      start_app_tolerate_peer "normal session after per-account verdict" || true
      ;;
    continue_normal)
      tg "⚠️ Probe inconclusive (base connection captured nothing) — infra/auth problem, not a limit answer. Continuing a NORMAL session; check the Groww token / feed page."
      stop_app
      apply_overlay none
      start_app_tolerate_peer "normal session after inconclusive probe" || true
      ;;
    none)
      tg "⚠️ Probe verdict TIMED OUT after ${PROBE_TIMEOUT_MIN}min — treating as inconclusive; continuing a NORMAL session. Check the app log + feed page."
      stop_app
      apply_overlay none
      start_app_tolerate_peer "normal session after probe timeout" || true
      ;;
    esac
  else
    apply_overlay none # normal local day: branch overlay (feeds ON) applies
    start_app_tolerate_peer "normal local day" || exit 1
    tg "🚀 Normal local day: app booted, both-feed capture live (dry_run — no orders)."
    spawn_feed_status_pings # FIX 19 item 7: one Telegram per feed, every mode
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
monitor) cmd_monitor ;;       # FIX 18 G5: detached safety net behind a manual Start
feed-pings) send_feed_status_pings "${2:-180}" ;; # FIX 19 item 7: detached per-feed Telegram status
status) cmd_status ;;
*)
  echo "usage: $0 [run|start|stop|dev|monitor|feed-pings|status]" >&2
  exit 2
  ;;
esac
