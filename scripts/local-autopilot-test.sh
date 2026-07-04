#!/usr/bin/env bash
# =============================================================================
# local-autopilot-test.sh — unit tests for the autopilot's PURE decision logic
# (day classification, time windows, probe-verdict parse, manual-override +
# monitor-loop brain). Run: bash scripts/local-autopilot-test.sh
# =============================================================================
set -u

cd "$(dirname "$0")/.."
# shellcheck source=scripts/local-autopilot.sh
AUTOPILOT_LIB=1 source scripts/local-autopilot.sh

PASS=0
FAIL=0
t() { # t <description> <expected> <actual>
  if [ "$2" = "$3" ]; then
    PASS=$((PASS + 1))
    echo "  ok   : $1"
  else
    FAIL=$((FAIL + 1))
    echo "  FAIL : $1 — expected '$2', got '$3'"
  fi
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# ── hhmm_to_secs / secs_until ───────────────────────────────────────────────
t "hhmm 09:45"           "35100" "$(hhmm_to_secs 09:45)"
t "hhmm 15:35:30"        "56130" "$(hhmm_to_secs 15:35:30)"
t "hhmm rejects 24:00"   ""      "$(hhmm_to_secs 24:00 || true)"
t "hhmm rejects garbage" ""      "$(hhmm_to_secs abc || true)"
t "secs_until future"    "3600"  "$(secs_until 10:00 09:00:00)"
t "secs_until exact"     "0"     "$(secs_until 10:00 10:00:00)"
t "secs_until passed"    "0"     "$(secs_until 09:45 15:00:00)"
t "secs_until bad target is 0 (fail-safe: no wait)" "0" "$(secs_until 9x:00 09:00:00)"

# ── holidays + day classification ───────────────────────────────────────────
cat >"$TMP/holidays.toml" <<'EOF'
[[trading.nse_holidays]]
date = "2026-07-07"
name = "Synthetic Test Holiday"
EOF

t "weekend Sat"          "weekend" "$(classify_day 2026-07-04 6 "$TMP/holidays.toml" 2026-07-06 2026-07-08)"
t "weekend Sun"          "weekend" "$(classify_day 2026-07-05 7 "$TMP/holidays.toml" 2026-07-06 2026-07-08)"
t "holiday beats scale"  "holiday" "$(classify_day 2026-07-07 2 "$TMP/holidays.toml" 2026-07-06 2026-07-08)"
t "scale window start"   "scale"   "$(classify_day 2026-07-06 1 "$TMP/holidays.toml" 2026-07-06 2026-07-08)"
t "scale window end"     "scale"   "$(classify_day 2026-07-08 3 "$TMP/holidays.toml" 2026-07-06 2026-07-08)"
t "after window normal"  "normal"  "$(classify_day 2026-07-09 4 "$TMP/holidays.toml" 2026-07-06 2026-07-08)"
t "missing holidays file → not holiday" "normal" "$(classify_day 2026-07-09 4 "$TMP/nope.toml" 2026-07-06 2026-07-08)"

# ── probe verdict parse (QuestDB /exec JSON shapes) ─────────────────────────
t "verdict present" "probe_multi_conn_ok" \
  "$(echo '{"query":"...","columns":[{"name":"reason"}],"dataset":[["probe_multi_conn_ok"]],"count":1}' | parse_probe_verdict)"
t "newest row wins (limit-1 dataset)" "probe_per_account_limited" \
  "$(echo '{"dataset":[["probe_per_account_limited"]]}' | parse_probe_verdict)"
t "empty dataset → empty" "" \
  "$(echo '{"dataset":[]}' | parse_probe_verdict)"
t "no dataset key → empty" "" \
  "$(echo '{"error":"table does not exist"}' | parse_probe_verdict)"
t "malformed JSON → empty (fail-quiet)" "" \
  "$(echo 'not json at all' | parse_probe_verdict)"
t "non-probe reason rejected" "" \
  "$(echo '{"dataset":[["gates_green"]]}' | parse_probe_verdict)"

# ── verdict → action decision tree ──────────────────────────────────────────
t "multi-conn → scale_max"      "scale_max"       "$(next_action_for_verdict probe_multi_conn_ok)"
t "per-account → record"        "record_continue" "$(next_action_for_verdict probe_per_account_limited)"
t "inconclusive → normal"       "continue_normal" "$(next_action_for_verdict probe_inconclusive)"
t "smoke verdict → normal"      "continue_normal" "$(next_action_for_verdict probe_smoke_machinery_validated)"
t "timeout/empty → none"        "none"            "$(next_action_for_verdict "")"
t "unknown label → none"        "none"            "$(next_action_for_verdict something_else)"

# ── manual-stop marker (manual always wins; stale markers expire) ──────────
printf '2026-07-06 10:15:00 manual stop\n' >"$TMP/marker"
t "marker today → active"    "0" "$(manual_stop_active "$TMP/marker" 2026-07-06 && echo 0 || echo 1)"
t "marker stale → inactive"  "1" "$(manual_stop_active "$TMP/marker" 2026-07-07 && echo 0 || echo 1)"
t "marker missing → inactive" "1" "$(manual_stop_active "$TMP/absent" 2026-07-06 && echo 0 || echo 1)"

# ── monitor-loop brain (app_alive, manual_stop, relaunches) ─────────────────
t "alive + no manual → idle"              "idle"             "$(monitor_decision 1 0 0)"
t "manual stop beats dead app (no fight)" "manual_standdown" "$(monitor_decision 0 1 0)"
t "manual stop beats alive app too"       "manual_standdown" "$(monitor_decision 1 1 0)"
t "dead app, first death → relaunch"      "relaunch"         "$(monitor_decision 0 0 0)"
t "dead app, second death → alert"        "alert"            "$(monitor_decision 0 0 1)"
t "dead app, already alerted → alert"     "alert"            "$(monitor_decision 0 0 2)"

# ── scale-window boundary logic ─────────────────────────────────────────────
t "in_scale_window inside"  "0" "$(in_scale_window 2026-07-07 2026-07-06 2026-07-08 && echo 0 || echo 1)"
t "in_scale_window before"  "1" "$(in_scale_window 2026-07-05 2026-07-06 2026-07-08 && echo 0 || echo 1)"
t "in_scale_window after"   "1" "$(in_scale_window 2026-07-09 2026-07-06 2026-07-08 && echo 0 || echo 1)"

# ── real repo holiday file sanity (Republic Day 2026 is listed) ────────────
t "real base.toml holiday hit" "holiday" "$(classify_day 2026-01-26 1 config/base.toml 2026-07-06 2026-07-08)"

# ── ONE log file (FIX 4): day-rollover filename + fixed-name symlink ───────
t "daily log path for a date" "data/logs/app.2026-07-04.log" "$(daily_app_log 2026-07-04)"
t "daily log rolls with the date (per-write recompute)" "data/logs/app.2026-07-05.log" "$(daily_app_log 2026-07-05)"
t "daily log garbage date → fail-quiet fixed name" "data/logs/app.unknown-date.log" "$(daily_app_log not-a-date)"
t "daily log empty date → fail-quiet fixed name" "data/logs/app.unknown-date.log" "$(daily_app_log "")"
# symlink helper: points the fixed name at today's file; re-points on rollover
mkdir -p "$TMP/logs"
touch "$TMP/logs/app.2026-07-04.log" "$TMP/logs/app.2026-07-05.log"
refresh_one_file_symlink "$TMP/logs/app.2026-07-04.log" "$TMP/logs/tickvault.log"
t "symlink points at today's file" "app.2026-07-04.log" "$(readlink "$TMP/logs/tickvault.log")"
refresh_one_file_symlink "$TMP/logs/app.2026-07-05.log" "$TMP/logs/tickvault.log"
t "symlink re-points on day rollover" "app.2026-07-05.log" "$(readlink "$TMP/logs/tickvault.log")"
t "symlink helper quiet on empty args" "0" "$(refresh_one_file_symlink "" "" && echo 0 || echo 1)"

# retention pruning (still used by the dated run-stamp files)
# retention: keep the newest 2 of 4 rotated files → oldest 2 listed for delete
DOOMED=$(rotated_logs_to_delete 2 \
  "d/autopilot.log.2026-07-01" "d/autopilot.log.2026-07-03" \
  "d/autopilot.log.2026-07-02" "d/autopilot.log.2026-07-04" | tr '\n' ' ')
t "retention deletes oldest beyond keep" "d/autopilot.log.2026-07-02 d/autopilot.log.2026-07-01 " "$DOOMED"
t "retention: under the cap deletes nothing" "" "$(rotated_logs_to_delete 7 "d/autopilot.log.2026-07-03" | tr -d '\n')"
t "retention: no files is a no-op" "" "$(rotated_logs_to_delete 7 | tr -d '\n')"

# ── mid-session disk re-check (2026-07-04 resilience fix) ──────────────────
t "disk_low: below floor"        "0" "$(disk_low 12 50 && echo 0 || echo 1)"
t "disk_low: at floor is OK"     "1" "$(disk_low 50 50 && echo 0 || echo 1)"
t "disk_low: above floor is OK"  "1" "$(disk_low 120 50 && echo 0 || echo 1)"
t "disk_low: probe failure (empty) is NOT low" "1" "$(disk_low "" 50 && echo 0 || echo 1)"
t "disk_low: probe garbage is NOT low" "1" "$(disk_low "N/A" 50 && echo 0 || echo 1)"
t "recheck due on multiple"      "0" "$(disk_recheck_due 30 30 && echo 0 || echo 1)"
t "recheck due on 2nd multiple"  "0" "$(disk_recheck_due 60 30 && echo 0 || echo 1)"
t "recheck NOT due off-multiple" "1" "$(disk_recheck_due 31 30 && echo 0 || echo 1)"
t "recheck disabled at 0"        "1" "$(disk_recheck_due 30 0 && echo 0 || echo 1)"

# ── docker VM memory probe helpers (2026-07-04 resilience fix) ─────────────
t "mem 8g → bytes"    "8589934592" "$(mem_limit_bytes 8g)"
t "mem 8G uppercase"  "8589934592" "$(mem_limit_bytes 8G)"
t "mem 512m → bytes"  "536870912"  "$(mem_limit_bytes 512m)"
t "mem 1024k → bytes" "1048576"    "$(mem_limit_bytes 1024k)"
t "mem raw bytes passthrough" "123456789" "$(mem_limit_bytes 123456789)"
t "mem garbage → empty (caller skips)" "" "$(mem_limit_bytes bananas || true)"
t "mem empty → empty (caller skips)"   "" "$(mem_limit_bytes "" || true)"
t "vm 16g >= pin 8g → sufficient"  "0" "$(docker_vm_mem_sufficient 17179869184 8589934592 && echo 0 || echo 1)"
t "vm 6g < pin 8g → insufficient"  "1" "$(docker_vm_mem_sufficient 6442450944 8589934592 && echo 0 || echo 1)"
t "vm exactly the pin → sufficient" "0" "$(docker_vm_mem_sufficient 8589934592 8589934592 && echo 0 || echo 1)"
t "vm garbage → insufficient (fail-quiet)" "1" "$(docker_vm_mem_sufficient "" 8589934592 && echo 0 || echo 1)"

# ── missed-run detection: prev_trading_day (2026-07-04 resilience fix) ─────
# 2026-07-06 is a Monday → previous trading day is Friday 2026-07-03.
t "prev trading day skips weekend" "2026-07-03" "$(prev_trading_day 2026-07-06 "$TMP/holidays.toml")"
# 2026-07-08 is a Wednesday; 2026-07-07 is the synthetic holiday → Monday 07-06.
t "prev trading day skips holiday" "2026-07-06" "$(prev_trading_day 2026-07-08 "$TMP/holidays.toml")"
# Plain weekday: Thursday 2026-07-02 → Wednesday 2026-07-01.
t "prev trading day plain weekday" "2026-07-01" "$(prev_trading_day 2026-07-02 "$TMP/holidays.toml")"
# Missing holidays file: still returns the calendar-previous weekday.
t "prev trading day tolerates missing holiday file" "2026-07-03" "$(prev_trading_day 2026-07-06 "$TMP/nope.toml")"
# Real repo calendar: Tue 2026-01-27 → Republic Day 01-26 is a holiday → Fri 01-23.
t "prev trading day skips Republic Day (real base.toml)" "2026-01-23" "$(prev_trading_day 2026-01-27 config/base.toml)"
# Garbage date → empty (caller skips the check).
t "prev trading day garbage date → empty" "" "$(prev_trading_day not-a-date "$TMP/holidays.toml")"

# ── QuestDB container self-heal: questdb_state_degraded (2026-07-04) ───────
t "qdb running healthy → not degraded"   "1" "$(questdb_state_degraded "running healthy" && echo 0 || echo 1)"
t "qdb running (no healthcheck) → not degraded" "1" "$(questdb_state_degraded "running " && echo 0 || echo 1)"
t "qdb running starting → not degraded (still warming)" "1" "$(questdb_state_degraded "running starting" && echo 0 || echo 1)"
t "qdb running unhealthy → degraded"     "0" "$(questdb_state_degraded "running unhealthy" && echo 0 || echo 1)"
t "qdb exited → degraded"                "0" "$(questdb_state_degraded "exited " && echo 0 || echo 1)"
t "qdb dead → degraded"                  "0" "$(questdb_state_degraded "dead " && echo 0 || echo 1)"
t "qdb paused → degraded"                "0" "$(questdb_state_degraded "paused " && echo 0 || echo 1)"
t "qdb restarting → not degraded (docker already on it)" "1" "$(questdb_state_degraded "restarting " && echo 0 || echo 1)"
t "qdb empty state → not degraded (no container = compose creates it)" "1" "$(questdb_state_degraded "" && echo 0 || echo 1)"

echo
echo "local-autopilot pure-logic tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
