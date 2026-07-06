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
# FIX 16 (F4): after the one alert the loop goes QUIET — the old behavior
# ("alert" forever) paged every 60s until 15:35 on a double-crash.
t "dead app, already alerted → quiet"     "idle_alerted"     "$(monitor_decision 0 0 2)"
t "dead app, long-alerted → still quiet"  "idle_alerted"     "$(monitor_decision 0 0 7)"

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

# ── FIX 5 (operator live-run bugs 2026-07-04 20:00 IST) ────────────────────
# 5.1 adopt-or-kill decision before app launch
t "adopt: found + healthy" "adopt" "$(adopt_or_kill_decision 1 1)"
t "kill: found + unhealthy" "kill" "$(adopt_or_kill_decision 1 0)"
t "launch: nothing found" "launch" "$(adopt_or_kill_decision 0 0)"
t "launch: healthy-but-no-proc is still launch" "launch" "$(adopt_or_kill_decision 0 1)"
t "launch: garbage args fail-safe to launch" "launch" "$(adopt_or_kill_decision "" "")"
# 5.1 first_pid — pick one pid out of pgrep output
t "first_pid: first of several" "123" "$(first_pid $'123\n456')"
t "first_pid: skips non-numeric lines" "77" "$(first_pid $'abc\n77')"
t "first_pid: empty input → empty" "" "$(first_pid "")"
# 5.1 early-exit classify
t "launch verdict: alive → ok" "ok" "$(app_launch_verdict 1 5 5)"
t "launch verdict: dead within window → early_exit" "early_exit" "$(app_launch_verdict 0 5 5)"
t "launch verdict: dead after window → died_later" "died_later" "$(app_launch_verdict 0 60 5)"
t "launch verdict: garbage secs fail-safe → died_later" "died_later" "$(app_launch_verdict 0 abc 5)"
# 5.4 Docker-VM memory clamp: floor(vm/1GiB) − 1, min 1g
t "clamp: 7GB VM → 6g" "6g" "$(clamp_qdb_mem_g 7516192768)"
t "clamp: 8GiB VM → 7g" "7g" "$(clamp_qdb_mem_g 8589934592)"
t "clamp: 2GiB VM → 1g" "1g" "$(clamp_qdb_mem_g 2147483648)"
t "clamp: 1GiB VM floors at 1g" "1g" "$(clamp_qdb_mem_g 1073741824)"
t "clamp: garbage input → rc 1 (keep configured limit)" "1" "$(clamp_qdb_mem_g garbage >/dev/null 2>&1 && echo 0 || echo 1)"
# 5.2 Telegram SSM parameter path — MUST match the app's own namespace
t "tg param path dev bot-token" "/tickvault/dev/telegram/bot-token" "$(tg_ssm_path dev bot-token)"
t "tg param path prod chat-id" "/tickvault/prod/telegram/chat-id" "$(tg_ssm_path prod chat-id)"
t "tg param path empty env defaults to prod (the app's default)" "/tickvault/prod/telegram/bot-token" "$(tg_ssm_path "" bot-token)"
t "tg param path never uses the /dlt namespace" "0" "$(case "$(tg_ssm_path dev bot-token)" in /dlt/*) echo 1 ;; *) echo 0 ;; esac)"

# ── FIX 8.1: launcher resolves the SSM env exactly like the app ─────────────
t "tg env: TV_ENVIRONMENT wins" "prod" "$(tg_env_resolve prod dev)"
t "tg env: falls back to ENVIRONMENT" "dev" "$(tg_env_resolve "" dev)"
t "tg env: both empty → prod (app default)" "prod" "$(tg_env_resolve "" "")"
t "tg env: only TV set" "staging" "$(tg_env_resolve staging "")"

# ── FIX 8.2/8.3: stale-adopt + recreated-DB guards ─────────────────────────
t "sha: match" "match" "$(sha_compare abc123 abc123)"
t "sha: mismatch" "mismatch" "$(sha_compare abc123 def456)"
t "sha: empty app sha → unknown" "unknown" "$(sha_compare "" def456)"
t "sha: literal unknown build → unknown" "unknown" "$(sha_compare unknown def456)"
t "db probe: missing table detected" "missing" "$(db_probe_verdict '{"query":"select 1 from ws_event_audit limit 1","error":"table does not exist [table=ws_event_audit]","position":15}')"
t "db probe: dataset present → ok" "ok" "$(db_probe_verdict '{"query":"...","columns":[{"name":"1"}],"dataset":[[1]],"count":1}')"
t "db probe: empty body → unknown" "unknown" "$(db_probe_verdict "")"
t "db probe: garbage → unknown" "unknown" "$(db_probe_verdict 'connection refused')"
t "market hours: 10:00 Tue → in session" "0" "$(in_market_hours_ist "$(hhmm_to_secs 10:00)" 2 && echo 0 || echo 1)"
t "market hours: 08:59 → out" "1" "$(in_market_hours_ist "$(hhmm_to_secs 08:59)" 2 && echo 0 || echo 1)"
t "market hours: 15:30 close → out" "1" "$(in_market_hours_ist "$(hhmm_to_secs 15:30)" 2 && echo 0 || echo 1)"
t "market hours: Saturday → out" "1" "$(in_market_hours_ist "$(hhmm_to_secs 10:00)" 6 && echo 0 || echo 1)"
t "market hours: garbage secs → out (restart allowed)" "1" "$(in_market_hours_ist abc 2 && echo 0 || echo 1)"
t "adopt: fresh build + tables ok → adopt" "adopt" "$(adopt_health_decision match ok 0)"
t "adopt: stale build off-hours → restart on new build" "restart_stale" "$(adopt_health_decision mismatch ok 0)"
t "adopt: stale build MID-MARKET → never restart, warn" "warn_stale" "$(adopt_health_decision mismatch ok 1)"
t "adopt: db recreated off-hours → restart to rebuild tables" "restart_db" "$(adopt_health_decision match missing 0)"
t "adopt: db recreated MID-MARKET → page, keep app" "warn_db" "$(adopt_health_decision match missing 1)"
t "adopt: db missing outranks stale build" "restart_db" "$(adopt_health_decision mismatch missing 0)"
t "adopt: unknown sha + unknown db → conservative adopt" "adopt" "$(adopt_health_decision unknown unknown 0)"
t "adopt: unknown sha, tables ok, mid-market → adopt" "adopt" "$(adopt_health_decision unknown ok 1)"

# ── FIX 9: one-shot probe-then-run marker (two buttons forever) ─────────────
t "probe once: marker == today, no stamp → run" "run" "$(probe_once_decision 2026-07-04 2026-07-04 0)"
t "probe once: marker != today → skip (inert Monday)" "skip" "$(probe_once_decision 2026-07-04 2026-07-06 0)"
t "probe once: stamp exists → skip (one attempt only)" "skip" "$(probe_once_decision 2026-07-04 2026-07-04 1)"
t "probe once: no marker → skip" "skip" "$(probe_once_decision "" 2026-07-04 0)"
t "probe once: malformed marker → skip" "skip" "$(probe_once_decision garbage 2026-07-04 0)"
t "probe once: missing stamp arg fail-safe → skip" "skip" "$(probe_once_decision 2026-07-04 2026-07-04 "")"

# FIX 11: attempt-numbered marker ("<date> N"; bare date = attempt 1)
t "marker date: bare date" "2026-07-04" "$(probe_marker_date "2026-07-04")"
t "marker date: date + attempt" "2026-07-04" "$(probe_marker_date "2026-07-04 2")"
t "marker attempt: bare date → 1" "1" "$(probe_marker_attempt "2026-07-04")"
t "marker attempt: explicit 2" "2" "$(probe_marker_attempt "2026-07-04 2")"
t "marker attempt: garbage → 1" "1" "$(probe_marker_attempt "2026-07-04 two")"
t "marker attempt: zero → 1" "1" "$(probe_marker_attempt "2026-07-04 0")"
t "marker attempt: empty line → 1" "1" "$(probe_marker_attempt "")"
# end-to-end: attempt-2 marker still gates on TODAY + its OWN stamp
t "attempt-2 marker: date matches, attempt-2 stamp absent → run" "run" \
  "$(probe_once_decision "$(probe_marker_date "2026-07-04 2")" 2026-07-04 0)"

# ── FIX 6: 100-conn probe helpers ───────────────────────────────────────────
# shellcheck source=scripts/groww-scale-test.sh
SCALE_TEST_LIB=1 source scripts/groww-scale-test.sh
# GATE_HOLD_MIN parse/validation (env override for fast weekend probes)
t "gate hold: 3 ok" "0" "$(valid_gate_hold_min 3 && echo 0 || echo 1)"
t "gate hold: 15 ok" "0" "$(valid_gate_hold_min 15 && echo 0 || echo 1)"
t "gate hold: 1440 ceiling ok" "0" "$(valid_gate_hold_min 1440 && echo 0 || echo 1)"
t "gate hold: 0 rejected" "1" "$(valid_gate_hold_min 0 && echo 0 || echo 1)"
t "gate hold: 1441 rejected" "1" "$(valid_gate_hold_min 1441 && echo 0 || echo 1)"
t "gate hold: empty rejected" "1" "$(valid_gate_hold_min "" && echo 0 || echo 1)"
t "gate hold: garbage rejected" "1" "$(valid_gate_hold_min abc && echo 0 || echo 1)"
t "gate hold: negative rejected" "1" "$(valid_gate_hold_min -3 && echo 0 || echo 1)"
# shellcheck source=scripts/scale-100-probe.sh
SCALE_PROBE_LIB=1 source scripts/scale-100-probe.sh
# probe verdict classifier (hit_ceiling, max_conns, target, real_subscribed)
# FIX 10 A2: ✅ demands REAL subscribe proof, never the smoke-hollow rung alone
t "verdict: ceiling + 100 REALLY subscribed → reached" "Reached 100" "$(scale_probe_verdict 1 100 100 100 | cut -c1-11)"
t "verdict: held rung but only 40 proven → NOT a pass" "Held rung 100, but only 40 of 100 connections REALLY subscribed" "$(scale_probe_verdict 1 100 100 40 | cut -c1-63)"
t "verdict: held-rung wording never shows the checkmark" "0" "$(case "$(scale_probe_verdict 1 100 100 40)" in *✅*) echo 1 ;; *) echo 0 ;; esac)"
t "verdict: capped at 40 names the rung + proof" "Groww (or this Mac) capped us at 40 of 100" "$(scale_probe_verdict 0 40 100 12 | cut -c1-42)"
t "verdict: ceiling flag but below target is still the cap answer" "Groww (or this Mac) capped us at 80 of 100" "$(scale_probe_verdict 1 80 100 80 | cut -c1-42)"
t "verdict: never climbed" "No ladder stage was recorded" "$(scale_probe_verdict 0 0 100 0 | cut -c1-28)"
t "verdict: garbage hit flag → rc 1" "1" "$(scale_probe_verdict 2 40 100 40 >/dev/null 2>&1 && echo 0 || echo 1)"
t "verdict: garbage max → rc 1" "1" "$(scale_probe_verdict 1 abc 100 1 >/dev/null 2>&1 && echo 0 || echo 1)"
t "verdict: missing proof arg → rc 1" "1" "$(scale_probe_verdict 1 100 100 >/dev/null 2>&1 && echo 0 || echo 1)"

# ── FIX 19 item 9: honest timer-cap verdict — a self-inflicted auto-stop
# must NEVER masquerade as "Groww capped us" (FIX 18 G15 class) ─────────────
t "F19-9: timer cap mid-climb → TIMER CAP verdict" "TIMER CAP HIT at rung 80 of 100" "$(scale_probe_verdict 0 80 100 80 1 0 | cut -c1-31)"
t "F19-9: timer-cap wording never blames Groww" "0" "$(case "$(scale_probe_verdict 0 80 100 80 1 0)" in *"capped us"*) echo 1 ;; *) echo 0 ;; esac)"
t "F19-9: timer-cap wording points at the cap knob" "0" "$(scale_probe_verdict 0 80 100 80 1 0 | grep -q 'PROBE_ONCE_MAX_MIN' && echo 0 || echo 1)"
t "F19-9: timer cap AFTER a rollback → legacy capped answer stands" "Groww (or this Mac) capped us at 40 of 100" "$(scale_probe_verdict 0 40 100 12 1 1 | cut -c1-42)"
t "F19-9: no timer cap → legacy capped wording unchanged" "Groww (or this Mac) capped us at 40 of 100" "$(scale_probe_verdict 0 40 100 12 0 0 | cut -c1-42)"
t "F19-9: timer cap but ceiling reached → reached wins" "Reached 100" "$(scale_probe_verdict 1 100 100 100 1 0 | cut -c1-11)"
t "F19-9: timer cap with zero rungs → never-climbed answer" "No ladder stage was recorded" "$(scale_probe_verdict 0 0 100 0 1 0 | cut -c1-28)"
t "F19-9: omitted grace args default to legacy behaviour" "Groww (or this Mac) capped us at 40 of 100" "$(scale_probe_verdict 0 40 100 12 | cut -c1-42)"
t "F19-9: garbage timer-cap flag → rc 1" "1" "$(scale_probe_verdict 0 40 100 12 2 0 >/dev/null 2>&1 && echo 0 || echo 1)"
t "F19-9: garbage rollback flag → rc 1" "1" "$(scale_probe_verdict 0 40 100 12 1 x >/dev/null 2>&1 && echo 0 || echo 1)"
# FIX 19 item 9: time-budget wiring pins (warm-up env + 75-min cap + probe pings)
t "F19-9: probe make target exports the 30s smoke warm-up" "1" "$(grep -cF 'GATE_WARMUP_SECS="$${GATE_WARMUP_SECS:-30}"' Makefile || true)"
t "F19-9: launcher probe cap default is 75 min" "1" "$(grep -c 'PROBE_ONCE_MAX_MIN:-75' scripts/local-autopilot.sh || true)"
t "F19-9: wrapper cap fallback is 75 min" "1" "$(grep -c 'PROBE_AUTO_STOP_MIN=75' scripts/scale-100-probe.sh || true)"
t "F19-9: stale 45-min defaults are gone" "0" "$(grep -c 'PROBE_ONCE_MAX_MIN:-45\|PROBE_AUTO_STOP_MIN=45' scripts/local-autopilot.sh scripts/scale-100-probe.sh | awk -F: '{s+=$NF} END {print s+0}')"
t "F19-9: banner estimate updated (no stale ~25 min claim)" "0" "$(grep -c '~25 min to 100' scripts/scale-100-probe.sh || true)"
t "F19-9: hard-cap branch latches TIMER_CAP_HIT" "1" "$(sed -n '/hard time cap reached/,/break/p' scripts/scale-100-probe.sh | grep -c 'TIMER_CAP_HIT=1' || true)"
t "F19-9: probe path spawns the feed pinger too" "1" "$(grep -c 'spawn_feed_status_pings # FIX 19 item 9' scripts/local-autopilot.sh || true)"

# ── FIX 10 (PUSH A): probe correctness + launcher safety helpers ────────────
# A1: TSV run-start sentinel normalization (macOS wc pads with spaces)
t "sentinel: padded wc output → clean int" "42" "$(tsv_sentinel "     42 ")"
t "sentinel: empty → 0" "0" "$(tsv_sentinel "")"
t "sentinel: garbage → 0" "0" "$(tsv_sentinel "abc")"
# A3: leftover scale overlay detection
printf '[feeds]\ndhan_enabled = false # scale-override(was: true)\n' >"$TMP/local-overlay.toml"
printf '# >>> groww-scale-test (managed by scripts/groww-scale-test.sh) >>>\n[feeds.groww.scale]\n' >"$TMP/local-block.toml"
printf '[feeds]\ndhan_enabled = true\n' >"$TMP/local-clean.toml"
t "overlay: in-place override detected" "0" "$(has_scale_overlay "$TMP/local-overlay.toml" && echo 0 || echo 1)"
t "overlay: managed block detected" "0" "$(has_scale_overlay "$TMP/local-block.toml" && echo 0 || echo 1)"
t "overlay: clean config → none" "1" "$(has_scale_overlay "$TMP/local-clean.toml" && echo 0 || echo 1)"
t "overlay: missing file → none" "1" "$(has_scale_overlay "$TMP/local-absent.toml" && echo 0 || echo 1)"
# A4: launcher lock staleness (pid check)
t "lock: our own live pid → NOT stale" "1" "$(launcher_lock_stale $$ && echo 0 || echo 1)"
t "lock: dead pid → stale" "0" "$(launcher_lock_stale 4194000 && echo 0 || echo 1)"
t "lock: empty pid → stale" "0" "$(launcher_lock_stale "" && echo 0 || echo 1)"
t "lock: garbage pid → stale" "0" "$(launcher_lock_stale abc && echo 0 || echo 1)"
# A8: operator Stop during probe (marker mtime vs probe start + grace)
t "stop-during-probe: marker 200s after start → operator stop" "0" "$(manual_stop_during_probe 1000200 1000000 && echo 0 || echo 1)"
t "stop-during-probe: marker 5s after start → probe's own stop" "1" "$(manual_stop_during_probe 1000005 1000000 && echo 0 || echo 1)"
t "stop-during-probe: marker exactly at grace edge → own stop" "1" "$(manual_stop_during_probe 1000120 1000000 && echo 0 || echo 1)"
t "stop-during-probe: garbage marker → fail-safe not-operator" "1" "$(manual_stop_during_probe abc 1000000 && echo 0 || echo 1)"
t "stop-during-probe: custom grace honored" "0" "$(manual_stop_during_probe 1000031 1000000 30 && echo 0 || echo 1)"

# ── FIX 7: Docker Desktop VM memory auto-raise (pure decision + JSON edit) ──
# decision: vm / required / preferred-target / host (all MiB)
t "vm raise: already sufficient → ok" "ok" "$(docker_vm_raise_target 10240 10240 10240 49152)"
t "vm raise: 7GB VM, 48GB host → raise to 10240" "10240" "$(docker_vm_raise_target 7168 10240 10240 49152)"
t "vm raise: required above preferred wins" "12288" "$(docker_vm_raise_target 7168 12288 10240 49152)"
t "vm raise: 16GB host too small (target > host/3)" "host_small" "$(docker_vm_raise_target 7168 10240 10240 16384)"
t "vm raise: garbage vm → rc 1" "1" "$(docker_vm_raise_target abc 10240 10240 49152 >/dev/null 2>&1 && echo 0 || echo 1)"
t "vm raise: empty host → rc 1" "1" "$(docker_vm_raise_target 7168 10240 10240 "" >/dev/null 2>&1 && echo 0 || echo 1)"
# JSON key pick (Docker Desktop >= 4.30 settings-store vs legacy settings)
printf '{"MemoryMiB": 7168, "Cpus": 8}\n' >"$TMP/settings-store.json"
printf '{"memoryMiB": 7168, "cpus": 8}\n' >"$TMP/settings.json"
printf '{"MemoryMiB": 7168, "memoryMiB": 7168}\n' >"$TMP/settings-both.json"
printf '{"cpus": 8}\n' >"$TMP/settings-nokey.json"
printf 'not json at all\n' >"$TMP/settings-bad.json"
t "settings key: new store file → MemoryMiB" "MemoryMiB" "$(docker_settings_mem_key "$TMP/settings-store.json")"
t "settings key: legacy file → memoryMiB" "memoryMiB" "$(docker_settings_mem_key "$TMP/settings.json")"
t "settings key: both present → new key wins" "MemoryMiB" "$(docker_settings_mem_key "$TMP/settings-both.json")"
# FIX 14: a valid dict with NO memory key prints the modern default and
# signals rc 10 (insert path) — the distinct failure rcs let the caller
# log file-absent / bad-JSON / python3-fail as separate messages.
t "settings key: no memory key → default MemoryMiB" "MemoryMiB" "$(docker_settings_mem_key "$TMP/settings-nokey.json" 2>/dev/null || true)"
t "settings key: no memory key → rc 10 (insert signal)" "10" "$(docker_settings_mem_key "$TMP/settings-nokey.json" >/dev/null 2>&1 && echo 0 || echo $?)"
t "settings key: bad JSON → rc 3" "3" "$(docker_settings_mem_key "$TMP/settings-bad.json" >/dev/null 2>&1 && echo 0 || echo $?)"
t "settings key: missing file → rc 2" "2" "$(docker_settings_mem_key "$TMP/settings-absent.json" >/dev/null 2>&1 && echo 0 || echo $?)"
# JSON round-trip edit (never osascript / never a live Docker in tests)
docker_settings_set_mem "$TMP/settings-store.json" MemoryMiB 10240
t "settings set: memory raised to 10240" "10240" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["MemoryMiB"])' "$TMP/settings-store.json")"
t "settings set: other keys preserved" "8" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["Cpus"])' "$TMP/settings-store.json")"
# FIX 14: absent key is INSERTED (fresh settings-store.json), other keys kept
docker_settings_set_mem "$TMP/settings-nokey.json" MemoryMiB 10240
t "settings set: absent key inserted (FIX 14)" "10240" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["MemoryMiB"])' "$TMP/settings-nokey.json")"
t "settings set: insert preserves other keys" "8" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["cpus"])' "$TMP/settings-nokey.json")"
t "settings set: garbage json → rc 1" "1" "$(docker_settings_set_mem "$TMP/settings-bad.json" MemoryMiB 10240 >/dev/null 2>&1 && echo 0 || echo 1)"
t "settings set: garbage json file untouched" "not json at all" "$(cat "$TMP/settings-bad.json")"
t "settings set: garbage mib → rc 1" "1" "$(docker_settings_set_mem "$TMP/settings.json" memoryMiB tenGB >/dev/null 2>&1 && echo 0 || echo 1)"
t "settings set: missing file → rc 1" "1" "$(docker_settings_set_mem "$TMP/settings-absent.json" memoryMiB 10240 >/dev/null 2>&1 && echo 0 || echo 1)"

# ── FIX 17: Docker memory — clamp-and-proceed, NEVER restart Docker ─────────
# Read-back helper (idempotence source for the hint)
t "mem get: reads written value" "10240" "$(docker_settings_mem_get "$TMP/settings-store.json" MemoryMiB)"
t "mem get: absent key → rc 1" "1" "$(docker_settings_mem_get "$TMP/settings.json" MemoryMiB >/dev/null 2>&1 && echo 0 || echo 1)"
t "mem get: missing file → rc 1" "1" "$(docker_settings_mem_get "$TMP/settings-absent.json" MemoryMiB >/dev/null 2>&1 && echo 0 || echo 1)"
t "mem get: bad JSON → rc 1" "1" "$(docker_settings_mem_get "$TMP/settings-bad.json" MemoryMiB >/dev/null 2>&1 && echo 0 || echo 1)"
# No-loop property: once the hint equals/exceeds the target it is NEVER
# rewritten — even though Docker Desktop 4.80 ignores the file, a later run
# sees the recorded value and skips (no re-fire, no restart, no loop).
t "mem hint: already at target → skip" "skip" "$(docker_mem_hint_decision 10240 10240)"
t "mem hint: above target → skip" "skip" "$(docker_mem_hint_decision 12288 10240)"
t "mem hint: below target → write" "write" "$(docker_mem_hint_decision 8192 10240)"
t "mem hint: no current value → write" "write" "$(docker_mem_hint_decision "" 10240)"
t "mem hint: garbage current → write" "write" "$(docker_mem_hint_decision abc 10240)"
t "mem hint: garbage target → skip (fail-quiet)" "skip" "$(docker_mem_hint_decision 8192 "")"
# Ratchets (FIX 17 → FIX 31): the default path is clamp-and-proceed; a
# Docker quit exists ONLY inside the FIX 31 cold-start cycle (app + database
# provably down before it may fire — never a mid-run restart); every
# 'open -a Docker' code site lives in either the FIX 31 cycle or the
# ensure-docker not-running start site; the hint log promises next-start
# semantics; the clamp remains wired.
t "FIX 31: exactly ONE Docker quit command in the whole script" "1" "$(grep -c "osascript -e 'quit app" scripts/local-autopilot.sh || true)"
t "FIX 31: that quit lives INSIDE fix31_auto_raise_docker_mem" "1" \
  "$(sed -n '/^fix31_auto_raise_docker_mem()/,/^}/p' scripts/local-autopilot.sh | grep -c "osascript -e 'quit app" || true)"
t "FIX 31: 'open -a Docker' code sites = 3 in fix31 + 1 at the not-running start site" "4" \
  "$(grep -cE '^[[:space:]]+open -a Docker' scripts/local-autopilot.sh || true)"
t "FIX 31: ensure_docker keeps its single not-running start site" "1" \
  "$(sed -n '/^ensure_docker()/,/^}/p' scripts/local-autopilot.sh | grep -c 'open -a Docker' || true)"
t "FIX 17: hint log promises next-start (no mid-run apply claim)" "1" "$(grep -c 'continuing now at current memory' scripts/local-autopilot.sh || true)"
t "FIX 17: clamp-and-proceed remains the primary path" "1" "$(grep -c 'auto-clamping QDB_MEM_LIMIT' scripts/local-autopilot.sh || true)"
t "FIX 17: hint call is non-blocking (|| true) in check_docker_vm_memory" "1" "$(sed -n '/^check_docker_vm_memory()/,/^}/p' scripts/local-autopilot.sh | grep -c 'hint_docker_vm_memory .* || true')"

# ── FIX 10 PUSH B: Monday-morning robustness pure helpers ───────────────────
# B2: bounded boot retry (retry while the 09:10 deadline has not passed)
t "boot retry: 300s left → retry" "0" "$(boot_retry_should_retry 300 && echo 0 || echo 1)"
t "boot retry: deadline passed (0s) → give up" "1" "$(boot_retry_should_retry 0 && echo 0 || echo 1)"
t "boot retry: garbage → give up (fail-safe)" "1" "$(boot_retry_should_retry abc && echo 0 || echo 1)"
t "boot retry: empty → give up (fail-safe)" "1" "$(boot_retry_should_retry '' && echo 0 || echo 1)"

# B5: PID-reuse guard (cmdline match + elapsed-time parse + start plausibility)
t "pid cmdline: our binary → ours" "0" "$(pid_cmdline_is_tickvault "./target/release/tickvault" && echo 0 || echo 1)"
t "pid cmdline: absolute path → ours" "0" "$(pid_cmdline_is_tickvault "/Users/op/tickvault/target/release/tickvault" && echo 0 || echo 1)"
t "pid cmdline: foreign program → not ours" "1" "$(pid_cmdline_is_tickvault "/usr/bin/python3 something.py" && echo 0 || echo 1)"
t "pid cmdline: empty → not ours" "1" "$(pid_cmdline_is_tickvault "" && echo 0 || echo 1)"
t "etime mm:ss" "125" "$(etime_to_secs 02:05)"
t "etime hh:mm:ss" "3725" "$(etime_to_secs 01:02:05)"
t "etime dd-hh:mm:ss" "93784" "$(etime_to_secs 1-02:03:04)"
t "etime leading zeros safe (08:09)" "489" "$(etime_to_secs 08:09)"
t "etime garbage → empty (skip plausibility check)" "" "$(etime_to_secs abc)"
t "etime empty → empty" "" "$(etime_to_secs '')"
t "pid start: before pidfile write → plausible" "0" "$(pid_start_plausible 1000000 1000300 && echo 0 || echo 1)"
t "pid start: within 60s slack after write → plausible" "0" "$(pid_start_plausible 1000050 1000000 && echo 0 || echo 1)"
t "pid start: 100s after pidfile write → recycled pid" "1" "$(pid_start_plausible 1000100 1000000 && echo 0 || echo 1)"
t "pid start: garbage start → fail-safe not-ours" "1" "$(pid_start_plausible abc 1000000 && echo 0 || echo 1)"
t "pid start: garbage mtime → fail-safe not-ours" "1" "$(pid_start_plausible 1000000 '' && echo 0 || echo 1)"

# B6: database-port ownership (docker inspect state → verdict)
t "qdb owner: running → ok" "ok" "$(questdb_owner_verdict "running healthy")"
t "qdb owner: running no-health → ok" "ok" "$(questdb_owner_verdict "running ")"
t "qdb owner: empty (no container, port answers) → foreign" "foreign" "$(questdb_owner_verdict "")"
t "qdb owner: exited container → not_running" "not_running" "$(questdb_owner_verdict "exited unhealthy")"

# ── FIX 15: final pre-Monday hardening ──────────────────────────────────────
# 15.1: fd-limit clamp
t "fd clamp: desired below hard → desired" "4096" "$(fd_limit_target 4096 10240)"
t "fd clamp: desired above hard → hard" "256" "$(fd_limit_target 4096 256)"
t "fd clamp: unlimited hard → desired" "4096" "$(fd_limit_target 4096 unlimited)"
t "fd clamp: garbage hard → rc 1" "1" "$(fd_limit_target 4096 abc >/dev/null 2>&1 && echo 0 || echo 1)"
t "fd clamp: empty desired → rc 1" "1" "$(fd_limit_target "" 256 >/dev/null 2>&1 && echo 0 || echo 1)"

# 15.6: probe failed-to-launch (zero new ladder rows)
t "probe launch: no new rows → failed-to-launch" "0" "$(probe_launch_failed 5 5 && echo 0 || echo 1)"
t "probe launch: fewer rows (rotated) → failed-to-launch" "0" "$(probe_launch_failed 5 3 && echo 0 || echo 1)"
t "probe launch: new rows → partial run (attempt stays burned)" "1" "$(probe_launch_failed 5 9 && echo 0 || echo 1)"
t "probe launch: garbage before → fail-safe not-failed" "1" "$(probe_launch_failed abc 9 && echo 0 || echo 1)"
t "probe launch: garbage after → fail-safe not-failed" "1" "$(probe_launch_failed 5 '' && echo 0 || echo 1)"

# 15.7: Telegram disable latch re-arm
t "tg latch: 61+ min old → expired" "0" "$(tg_latch_expired 10000 6300 3600 && echo 0 || echo 1)"
t "tg latch: 30 min old → not expired" "1" "$(tg_latch_expired 10000 8200 3600 && echo 0 || echo 1)"
t "tg latch: exactly retry_secs → expired" "0" "$(tg_latch_expired 10000 6400 3600 && echo 0 || echo 1)"
t "tg latch: garbage disabled_at → expired (retry, not dead-all-day)" "0" "$(tg_latch_expired 10000 '' 3600 && echo 0 || echo 1)"
t "tg latch: garbage now → not expired" "1" "$(tg_latch_expired abc 6300 3600 && echo 0 || echo 1)"

# 15.8: Docker VM disk threshold
t "vm disk: 90% > 85 → high" "0" "$(vm_disk_high 90 85 && echo 0 || echo 1)"
t "vm disk: 85% (at threshold) → not high" "1" "$(vm_disk_high 85 85 && echo 0 || echo 1)"
t "vm disk: 50% → not high" "1" "$(vm_disk_high 50 85 && echo 0 || echo 1)"
t "vm disk: empty probe → not high (no warning on unreadable)" "1" "$(vm_disk_high "" 85 && echo 0 || echo 1)"
t "vm disk: garbage probe → not high" "1" "$(vm_disk_high '9x' 85 && echo 0 || echo 1)"

# 15.9: effective dhan_enabled read (local overrides base; no-spaces variant)
printf '[feeds]\ndhan_enabled = true\n' >"$TMP/f15-base.toml"
printf '[feeds]\ndhan_enabled=false\n' >"$TMP/f15-local.toml"
printf '[feeds]\ngroww_enabled = true\n' >"$TMP/f15-nokey.toml"
printf '[feeds]\ndhan_enabled = true # comment\n' >"$TMP/f15-comment.toml"
t "dhan flag: local false overrides base true" "false" "$(dhan_enabled_from_files "$TMP/f15-local.toml" "$TMP/f15-base.toml")"
t "dhan flag: local silent → base wins" "true" "$(dhan_enabled_from_files "$TMP/f15-nokey.toml" "$TMP/f15-base.toml")"
t "dhan flag: no-spaces variant parsed" "false" "$(dhan_enabled_from_files "$TMP/f15-local.toml")"
t "dhan flag: trailing comment stripped" "true" "$(dhan_enabled_from_files "$TMP/f15-comment.toml")"
t "dhan flag: both files absent → false" "false" "$(dhan_enabled_from_files "$TMP/f15-absent-a.toml" "$TMP/f15-absent-b.toml")"

# 15.4: own-process-group launch — functional pgid check + source ordering pins
pg_self=$(ps -o pgid= -p $$ | tr -d '[:space:]')
pg_child=$( (
  set -m
  sleep 1 &
  ps -o pgid= -p $! | tr -d '[:space:]'
  kill %1 2>/dev/null
) )
t "set -m: background job gets its OWN process group" "differs" "$([ -n "$pg_child" ] && [ "$pg_child" != "$pg_self" ] && echo differs || echo "same:$pg_child/$pg_self")"
t "launch: set -m precedes the nohup app launch (FIX 15.4)" "1" "$(grep -B4 'nohup \./target/release/tickvault' scripts/local-autopilot.sh | grep -c 'set -m')"
# 15.3: the manual-stop re-check sits BETWEEN cargo build and the nohup launch
build_ln=$(grep -n 'cargo build --release' scripts/local-autopilot.sh | head -1 | cut -d: -f1)
launch_ln=$(grep -n 'nohup \./target/release/tickvault' scripts/local-autopilot.sh | head -1 | cut -d: -f1)
stop_between=$(awk -v a="$build_ln" -v b="$launch_ln" 'NR>a && NR<b && /manual_stop_active/ {found=1} END {print found+0}' scripts/local-autopilot.sh)
t "stop-mid-build: marker re-check between build and launch (FIX 15.3)" "1" "$stop_between"
# 15.5: the IntelliJ dev path carries the self-update (fetch + ff-only,
# retried once per FIX 16 F3)
t "cmd_dev: self-update fetch present (FIX 15.5)" "1" "$(sed -n '/^cmd_dev()/,/^}/p' scripts/local-autopilot.sh | grep -c 'git fetch origin local-runtime')"
t "cmd_dev: ff-only merge present (FIX 15.5/16.3)" "1" "$(sed -n '/^cmd_dev()/,/^}/p' scripts/local-autopilot.sh | grep -c 'git_ff_merge_with_retry origin/local-runtime')"
# 15.7: the Telegram send curl is time-bounded
t "notify-telegram: curl has --max-time (FIX 15.7)" "1" "$(grep -c 'curl -s --max-time 10 --connect-timeout 5' scripts/notify-telegram.sh)"

# ── FIX 16: delta-audit confirmed defects (shell batch F1-F12, F16, F17) ────
# F9 (CRITICAL): a dead tv-questdb must never abort the monitor loop via
# set -e + pipefail — the probe is total (rc 0, empty output on failure).
t "F9: VM-disk probe never aborts under set -euo pipefail" "survived" "$( (set -euo pipefail; v=$(docker_vm_disk_pct); echo survived) 2>/dev/null || echo died)"
t "F9: probe body carries the || true guard" "1" "$(sed -n '/^docker_vm_disk_pct()/,/^}/p' scripts/local-autopilot.sh | grep -c '|| true')"
# F2: the start_app lock-skip path returns a DISTINCT rc (never a false 0)
t "F2: start_app lock-skip returns rc 3" "1" "$(sed -n '/^start_app()/,/^}/p' scripts/local-autopilot.sh | grep -c 'return 3')"
t "F2: peer-tolerant wrapper exists" "0" "$(grep -q '^start_app_tolerate_peer()' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F2: cmd_start words the peer case honestly" "0" "$(grep -q 'Another launcher is already starting the app' scripts/local-autopilot.sh && echo 0 || echo 1)"
# (8 sites since FIX 18 G16: the skip-max normal-session fallback added one.)
t "F2: robot day flow uses the peer-tolerant wrapper (9 since FIX 30 auto-probe resume)" "9" "$(grep -c 'start_app_tolerate_peer "' scripts/local-autopilot.sh || true)"
# F1: BOTH git writers serialize (acquire-or-skip) against a lock-held build
t "F1: both git writers take the launcher lock (acquire-or-skip)" "2" "$(grep -c 'launcher_lock_acquire_wait 5' scripts/local-autopilot.sh || true)"
t "F1: cmd_dev releases the lock BEFORE the re-exec" "1" "$(sed -n '/^cmd_dev()/,/^}/p' scripts/local-autopilot.sh | grep -c 'launcher_lock_release # MUST release before exec')"
# F3: transient index.lock loser gets one retry in both writers
t "F3: merge-with-retry used by both writers" "2" "$(grep -c 'git_ff_merge_with_retry origin/local-runtime' scripts/local-autopilot.sh || true)"
# F5: caffeinate survives an IDE group-kill (own process group) + monitor re-arm
t "F5: caffeinate launched in its own process group" "1" "$(sed -n '/^start_caffeinate()/,/^}/p' scripts/local-autopilot.sh | grep -c 'set -m')"
t "F5: monitor loop re-arms caffeinate (2 since FIX 30: per-cycle + probe resume)" "2" "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -c 'start_caffeinate')"
# F10/F11: ensure_questdb alerts + restart budget are per-EPISODE latched
t "F10: docker restart budget is one per outage episode" "0" "$(grep -q 'QDB_EPISODE_RESTARTED=1' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F10: give-up page latched per episode" "0" "$(grep -q 'QDB_EPISODE_PAGED=1' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F11: foreign-responder page latched per episode" "0" "$(grep -q 'QDB_FOREIGN_PAGED=1' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F10/F11: latches re-arm on recovery" "0" "$(sed -n '/^ensure_questdb()/,/^}/p' scripts/local-autopilot.sh | grep -q 'QDB_FOREIGN_PAGED=0' && echo 0 || echo 1)"
# F12: the mid-session liveness probe is debounced (2 consecutive samples)
t "F12: qdb probe debounced before heal/page" "0" "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'qdb_fail_streak < 2' && echo 0 || echo 1)"
# F6/F7: wrapper exit-code contract classifier (pure)
t "F6/F7: rc 0 → complete" "complete" "$(probe_rc_class 0)"
t "F6/F7: rc 130 (Ctrl-C) → interrupted (stays burned)" "interrupted" "$(probe_rc_class 130)"
t "F6/F7: rc 143 (TERM) → interrupted (stays burned)" "interrupted" "$(probe_rc_class 143)"
t "F6/F7: rc 20 → never_launched (safe to un-burn)" "never_launched" "$(probe_rc_class 20)"
t "F6/F7: rc 2 → other (evidence decides)" "other" "$(probe_rc_class 2)"
t "F6/F7: garbage rc → other (fail-safe)" "other" "$(probe_rc_class "")"
# F6/F7/F8: the wrapper carries the signal traps, the rc-20 arm, and the
# kill-fleet-on-exit sweep (a TERM'd wrapper must never leave the Dhan-OFF
# 100-conn ladder running for the next start to adopt).
t "F7: wrapper traps INT distinctly" "1" "$(grep -c "trap 'cleanup 130' INT" scripts/scale-100-probe.sh || true)"
t "F7: wrapper traps TERM distinctly" "1" "$(grep -c "trap 'cleanup 143' TERM" scripts/scale-100-probe.sh || true)"
# (2 sites since FIX 18 G15: zero-evidence rows + failed pre-build — both
#  are the never-launched class, safe to un-burn the attempt.)
t "F6: wrapper exits 20 on the never-launched class" "2" "$(grep -c 'exit 20' scripts/scale-100-probe.sh || true)"
t "F8: wrapper cleanup stops the ladder app" "0" "$(sed -n '/^cleanup()/,/^}/p' scripts/scale-100-probe.sh | grep -q 'pkill -TERM -f' && echo 0 || echo 1)"
# F16: the direct probe entries raise the open-files limit
t "F16: probe wrapper raises the fd limit" "1" "$(grep -c '^raise_fd_limit' scripts/scale-100-probe.sh || true)"
t "F16: groww-scale-test raises the fd limit before exec" "0" "$(grep -q 'ulimit -n "\$fd_target"' scripts/groww-scale-test.sh && echo 0 || echo 1)"
# F17: the clamped-below-need path logs (no silent third outcome)
t "F17: clamped hard-limit path logs a warning" "0" "$(sed -n '/^raise_fd_limit()/,/^}/p' scripts/local-autopilot.sh | grep -q 'cannot raise further' && echo 0 || echo 1)"

# ── FIX 18: real-environment permutation batch (G1-G16, 2026-07-05) ─────────

# G1: timeout runner selection + timed-out rc classification (pure)
t "G1: runner prefers timeout" "timeout" "$(timeout_runner 1 1 1)"
t "G1: runner falls back to gtimeout (macOS coreutils)" "gtimeout" "$(timeout_runner 0 1 1)"
t "G1: runner falls back to the perl alarm shim" "perl" "$(timeout_runner 0 0 1)"
t "G1: no runner at all → none (unbounded, logged)" "none" "$(timeout_runner 0 0 0)"
t "G1: rc 124 is a timeout" "0" "$(timed_out_rc 124 && echo 0 || echo 1)"
t "G1: rc 142 (128+SIGALRM, perl shim) is a timeout" "0" "$(timed_out_rc 142 && echo 0 || echo 1)"
t "G1: rc 1 is not a timeout" "1" "$(timed_out_rc 1 && echo 0 || echo 1)"
t "G1: garbage rc is not a timeout" "1" "$(timed_out_rc "" && echo 0 || echo 1)"
# G1 source scans: the monitor-loop docker probes are all bounded via dk
t "G1: docker_alive goes through dk" "0" "$(sed -n '/^docker_alive()/,/}/p' scripts/local-autopilot.sh | grep -q 'dk info' && echo 0 || echo 1)"
t "G1: container-state probe goes through dk" "0" "$(sed -n '/^questdb_container_state()/,/^}/p' scripts/local-autopilot.sh | grep -q 'dk inspect' && echo 0 || echo 1)"
t "G1: VM-disk probe goes through dk" "0" "$(sed -n '/^docker_vm_disk_pct()/,/^}/p' scripts/local-autopilot.sh | grep -q 'dk exec' && echo 0 || echo 1)"
t "G1: compose up is bounded (dk --long)" "0" "$(grep -q 'dk --long compose' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "G1: no bare 'docker restart' command remains" "0" "$(grep -cE '^[[:space:]]*docker restart' scripts/local-autopilot.sh || true)"
t "G1: wedged-daemon page is marker-file latched" "0" "$(grep -q 'DOCKER_WEDGED_MARK' scripts/local-autopilot.sh && echo 0 || echo 1)"

# G2/G6/G11: fresh Docker Desktop first-launch modal detection (pure)
t "G2: app up, never initialized → first_launch" "first_launch" "$(docker_start_failure_hint 1 0)"
t "G2: app up, settings exist → app_stuck" "app_stuck" "$(docker_start_failure_hint 1 1)"
t "G2: app not running → not_running" "not_running" "$(docker_start_failure_hint 0 0)"
t "G2: ensure_docker names the Accept step on first launch" "0" "$(sed -n '/^ensure_docker()/,/^}/p' scripts/local-autopilot.sh | grep -q 'service agreement' && echo 0 || echo 1)"
t "G2: ensure-ready launches Docker FOREGROUND (modal visible)" "0" "$(grep -c 'open -a Docker --background' scripts/ensure-ready.sh || true)"
t "G2: ensure-ready names the first-launch state on timeout" "0" "$(grep -q 'FIRST-TIME install' scripts/ensure-ready.sh && echo 0 || echo 1)"

# G3/G7/G12: compose creds — app-parity env resolve + success-only latch
t "G3/G7: compose creds resolve env like the app (tg_env_resolve)" "0" "$(sed -n '/^export_compose_creds()/,/^}/p' scripts/local-autopilot.sh | grep -q 'tg_env_resolve' && echo 0 || echo 1)"
t "G3/G7: compose creds fall back to prod (tg_init parity)" "0" "$(sed -n '/^export_compose_creds()/,/^}/p' scripts/local-autopilot.sh | grep -q 'candidates="\$env prod"' && echo 0 || echo 1)"
t "G3/G7: the dev default is gone from the creds path" "0" "$(sed -n '/^export_compose_creds()/,/^}/p' scripts/local-autopilot.sh | grep -c 'ENVIRONMENT:-dev' || true)"
t "G12: creds latch is set ONLY on success (2 success sites, no unconditional)" "2" "$(sed -n '/^export_compose_creds()/,/^}/p' scripts/local-autopilot.sh | grep -c 'COMPOSE_CREDS_DONE=1' || true)"
t "G12: no latch-set as the unconditional second statement" "0" "$(sed -n '/^export_compose_creds()/,/^}/p' scripts/local-autopilot.sh | sed -n '3p' | grep -c 'COMPOSE_CREDS_DONE=1' || true)"

# G4/G9/G10: install-time wake + log dir + launchd PATH (source scans)
t "G4: installer schedules the 08:50 auto-wake (best-effort)" "0" "$(grep -q 'pmset repeat wakeorpoweron MTWRF 08:50:00' Makefile && echo 0 || echo 1)"
t "G4: Start button schedules the auto-wake too" "0" "$(grep -q 'pmset repeat wakeorpoweron' 'Start TickVault.command' && echo 0 || echo 1)"
t "G4: uninstall cancels the auto-wake" "0" "$(grep -q 'pmset repeat cancel' Makefile && echo 0 || echo 1)"
t "G9: Makefile install creates data/logs before bootstrap" "0" "$(sed -n '/^local-autopilot-install:/,/^$/p' Makefile | grep -q 'data/logs' && echo 0 || echo 1)"
t "G9: Start button creates data/logs before bootstrap" "0" "$(grep -q 'mkdir -p data/local-autopilot data/logs' 'Start TickVault.command' && echo 0 || echo 1)"
t "G9: runbook documents clone -b local-runtime" "0" "$(grep -q 'git clone -b local-runtime' README-LOCAL.md && echo 0 || echo 1)"
t "G10: plist PATH carries ~/.docker/bin" "0" "$(grep -q '__HOME__/.docker/bin' deploy/local/com.tickvault.local-autopilot.plist.template && echo 0 || echo 1)"
t "G10: plist PATH carries Docker.app Resources/bin" "0" "$(grep -q '/Applications/Docker.app/Contents/Resources/bin' deploy/local/com.tickvault.local-autopilot.plist.template && echo 0 || echo 1)"
t "G10: ensure_docker probes ~/.docker/bin before 'not found'" "0" "$(sed -n '/^ensure_docker()/,/^}/p' scripts/local-autopilot.sh | grep -q '.docker/bin' && echo 0 || echo 1)"
t "G1: plist has the 09:05 weekday catch-up fires" "5" "$(grep -c '<integer>9</integer><key>Minute</key><integer>5</integer>' deploy/local/com.tickvault.local-autopilot.plist.template || true)"

# G1 compounding: run-lock takeover decision (pure)
t "G1c: same-day live holder → exit (genuine duplicate)" "exit" "$(run_lock_takeover 1 2026-07-06 2026-07-06)"
t "G1c: previous-day live holder → steal (wedged run)" "steal" "$(run_lock_takeover 1 2026-07-03 2026-07-06)"
t "G1c: dateless live holder → exit (conservative)" "exit" "$(run_lock_takeover 1 "" 2026-07-06)"
t "G1c: dead holder → reclaim" "reclaim" "$(run_lock_takeover 0 2026-07-03 2026-07-06)"
t "G1c: cmd_run writes the date into the lock" "0" "$(grep -q '"$TODAY" >"$LOCKDIR/pid"' scripts/local-autopilot.sh && echo 0 || echo 1)"

# G5: manual-start background monitor (pure decision + wiring scans)
t "G5: nothing covers the day → spawn" "spawn" "$(manual_monitor_needed 0 0 3600)"
t "G5: robot alive → skip" "skip" "$(manual_monitor_needed 1 0 3600)"
t "G5: monitor already running → skip" "skip" "$(manual_monitor_needed 0 1 3600)"
t "G5: past EOD → skip (evening session runs until Stop)" "skip" "$(manual_monitor_needed 0 0 0)"
t "G5: garbage secs → skip (fail-safe)" "skip" "$(manual_monitor_needed 0 0 abc)"
t "G5: cmd_start spawns the monitor" "0" "$(sed -n '/^cmd_start()/,/^}/p' scripts/local-autopilot.sh | grep -q 'maybe_spawn_manual_monitor' && echo 0 || echo 1)"
t "G5: cmd_stop stops the monitor (Stop always wins)" "0" "$(sed -n '/^cmd_stop()/,/^}/p' scripts/local-autopilot.sh | grep -q 'stop_manual_monitor' && echo 0 || echo 1)"
t "G5: cmd_run takes the day over from a manual monitor" "0" "$(sed -n '/^cmd_run()/,/^}/p' scripts/local-autopilot.sh | grep -q 'stop_manual_monitor' && echo 0 || echo 1)"
t "G5: monitor subcommand is dispatchable" "0" "$(grep -q '^monitor) cmd_monitor ;;' scripts/local-autopilot.sh && echo 0 || echo 1)"

# G8/G14: caffeinate floor (pure)
t "G8: after-15:45 start still arms the 4h floor" "14400" "$(caffeinate_secs 15:45 16:00:00 14400)"
t "G8: until-target wins when larger than the floor" "21600" "$(caffeinate_secs 15:45 09:45:00 14400)"
t "G8: garbage floor degrades to plain until-target" "3600" "$(caffeinate_secs 10:00 09:00:00 abc)"
t "G8: zero floor + passed target → 0 (legacy behavior reachable)" "0" "$(caffeinate_secs 15:45 16:00:00 0)"
t "G8: start_caffeinate uses the floor helper" "0" "$(sed -n '/^start_caffeinate()/,/^}/p' scripts/local-autopilot.sh | grep -q 'caffeinate_secs' && echo 0 || echo 1)"

# G13: image-pull failure classification (pure) + bounded git fetch
t "G13: 429/toomanyrequests → rate_limit" "rate_limit" "$(pull_failure_class 'toomanyrequests: You have reached your pull rate limit')"
t "G13: DNS lookup failure → offline" "offline" "$(pull_failure_class 'dial tcp: lookup registry-1.docker.io: no such host')"
t "G13: unknown manifest → other" "other" "$(pull_failure_class 'manifest unknown')"
t "G13: empty output → other" "other" "$(pull_failure_class "")"
t "G13: ensure_questdb pre-pulls the image explicitly" "0" "$(sed -n '/^ensure_questdb()/,/^}/p' scripts/local-autopilot.sh | grep -q 'pull_failure_class' && echo 0 || echo 1)"
t "G13: git fetch is time-bounded (both sites)" "2" "$(grep -c 'with_timeout "\$GIT_NET_TIMEOUT_SECS" git fetch' scripts/local-autopilot.sh || true)"

# G15: probe pre-build outside the hard time cap (source scans)
t "G15: launcher pre-builds before the probe" "0" "$(sed -n '/^maybe_run_probe_once()/,/^}/p' scripts/local-autopilot.sh | grep -q 'cargo build --release' && echo 0 || echo 1)"
t "G15: probe wrapper builds a missing binary before its timer" "0" "$(grep -q 'BEFORE the probe timer starts' scripts/scale-100-probe.sh && echo 0 || echo 1)"

# G16: clamped-memory max-ladder gate + OOM naming (pure + scans)
t "G16: clamped run skips the max ladder" "skip_max" "$(scale_max_gate 1)"
t "G16: unclamped run allows max" "max" "$(scale_max_gate 0)"
t "G16: garbage clamp flag allows max (default 0)" "max" "$(scale_max_gate "")"
t "G16: OOMKilled true detected" "0" "$(oomkilled_is_true 'true' && echo 0 || echo 1)"
t "G16: OOMKilled false rejected" "1" "$(oomkilled_is_true 'false' && echo 0 || echo 1)"
t "G16: OOMKilled with whitespace detected" "0" "$(oomkilled_is_true ' true
' && echo 0 || echo 1)"
t "G16: the scale branch consults the gate" "0" "$(grep -q 'scale_max_gate "\${QDB_MEM_CLAMPED:-0}"' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "G16: the clamp sets the flag" "0" "$(grep -q 'QDB_MEM_CLAMPED=1' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "G16: the 'works fine' false-OK wording is gone" "0" "$(grep -c 'this works fine' scripts/local-autopilot.sh || true)"
t "G16: ensure_questdb names an OOM kill as OOM" "0" "$(sed -n '/^ensure_questdb()/,/^}/p' scripts/local-autopilot.sh | grep -q 'oomkilled_is_true' && echo 0 || echo 1)"

# ── FIX 19: live-probe follow-ups (2026-07-05) ───────────────────────────────

# Item 2: tsv_rows — missing file is 0 with NO stderr noise
FIX19_TMP=$(mktemp -d)
printf 'a\nb\nc\n' >"$FIX19_TMP/three.tsv"
t "F19-2: tsv_rows counts an existing file" "3" "$(tsv_rows "$FIX19_TMP/three.tsv")"
t "F19-2: tsv_rows missing file → 0" "0" "$(tsv_rows "$FIX19_TMP/absent.tsv")"
t "F19-2: tsv_rows empty arg → 0" "0" "$(tsv_rows "")"
t "F19-2: tsv_rows missing file emits NO stderr" "" "$(tsv_rows "$FIX19_TMP/absent.tsv" 2>&1 >/dev/null)"
rm -rf "$FIX19_TMP"
# the bare `wc -l <` pattern must be gone from BOTH probe-facing scripts
t "F19-2: no bare wc redirection remains on the summary TSV" "0" "$(grep -c 'wc -l <"data/groww-scale/summary' scripts/local-autopilot.sh || true)"
t "F19-2: probe wrapper uses tsv_rows" "0" "$(grep -q 'TSV_SENTINEL=$(tsv_rows "$TSV")' scripts/scale-100-probe.sh && echo 0 || echo 1)"

# Item 3: probe-prep stop wording
t "F19-3: probe-prep flag → probe_prep" "probe_prep" "$(stop_notice_mode 1)"
t "F19-3: unset flag → manual" "manual" "$(stop_notice_mode "")"
t "F19-3: zero flag → manual" "manual" "$(stop_notice_mode 0)"
t "F19-3: garbage flag → manual" "manual" "$(stop_notice_mode banana)"
t "F19-3: cmd_stop consults the notice mode" "0" "$(sed -n '/^cmd_stop()/,/^}/p' scripts/local-autopilot.sh | grep -q 'stop_notice_mode' && echo 0 || echo 1)"
t "F19-3: probe-prep Telegram says the live start follows" "0" "$(grep -q 'the live start follows automatically' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F19-3: probe wrapper exports the probe-prep flag before the stop" "0" "$(grep -q 'export AUTOPILOT_PROBE_PREP=1' scripts/scale-100-probe.sh && echo 0 || echo 1)"

# Item 4: sidecar empty NATS error enrichment + coalescing (source scans;
# the functional coverage lives in scripts/groww-sidecar/test_dedup.py)
t "F19-4: sidecar carries the NATS error filter class" "0" "$(grep -q 'class NatsErrorDetailFilter' scripts/groww-sidecar/groww_sidecar.py && echo 0 || echo 1)"
t "F19-4: the filter is installed at hook time" "0" "$(grep -q 'install_sdk_error_filter(secrets)' scripts/groww-sidecar/groww_sidecar.py && echo 0 || echo 1)"
t "F19-4: coalescing cadence is 30" "0" "$(grep -q 'NATS_ERROR_LOG_EVERY = 30' scripts/groww-sidecar/groww_sidecar.py && echo 0 || echo 1)"

# Item 5: attempt-2 re-arm marker
# FIX 25 superseded the F19-5 armed-marker pin (attempt 2 complete →
# disarmed). FIX 30 (2026-07-05) superseded AGAIN (armed for Tue
# 2026-07-07). Operator demand 2026-07-06: the exam runs TODAY — the
# marker is re-armed as "2026-07-06 3" for the Monday live ladder run —
# the robot auto-fires it in the 10:00-13:30 IST window, and the
# per-attempt stamp keeps it one-shot. After the run, the disarm commit
# flips this pin back to the comment form (the F25/F30 either-or formedness
# pin below survives both states).
t "F19-5→F30: probe-once marker is armed for Mon 2026-07-06 attempt 3" "1 0 3" \
  "$(probe_marker_state deploy/local/probe-once.date 2026-07-06 "$TMP/f19-5-no-stamps")"

# Addendum: raise Docker VM memory at its NATURAL cold start
t "F19-A: mem target on a 48GB host → 12288" "12288" "$(docker_mem_target_mib $((48 * 1024 * 1024 * 1024)))"
t "F19-A: mem target exactly 32GB → 12288" "12288" "$(docker_mem_target_mib $((32 * 1024 * 1024 * 1024)))"
t "F19-A: mem target on a 16GB host → 10240" "10240" "$(docker_mem_target_mib $((16 * 1024 * 1024 * 1024)))"
t "F19-A: mem target garbage → 10240" "10240" "$(docker_mem_target_mib banana)"
t "F19-A: mem target empty → 10240" "10240" "$(docker_mem_target_mib "")"
t "F19-A: cpu target on 14 cores → 10" "10" "$(docker_cpu_target_cores 14)"
t "F19-A: cpu target on 12 cores → 8" "8" "$(docker_cpu_target_cores 12)"
t "F19-A: cpu target on 8 cores → half" "4" "$(docker_cpu_target_cores 8)"
t "F19-A: cpu target on 2 cores → floor 2" "2" "$(docker_cpu_target_cores 2)"
t "F19-A: cpu target garbage → 0 (skip)" "0" "$(docker_cpu_target_cores banana)"
t "F19-A: write only when daemon down AND app not running" "write" "$(cold_start_mem_write_decision 0 0)"
t "F19-A: app running → skip" "skip" "$(cold_start_mem_write_decision 0 1)"
t "F19-A: daemon up → skip" "skip" "$(cold_start_mem_write_decision 1 0)"
t "F19-A: garbage inputs → skip (fail-quiet)" "skip" "$(cold_start_mem_write_decision "" "")"
# wiring scans: the raise fires ONLY on the daemon-down launch path, backs
# the file up first, and the post-start log proves whether it stuck
t "F19-A: ensure_docker calls the cold-start raise before open -a Docker" "0" "$(sed -n '/^ensure_docker()/,/^}/p' scripts/local-autopilot.sh | grep -q 'raise_docker_mem_at_cold_start' && echo 0 || echo 1)"
t "F19-A: the raise never runs while Docker.app is up" "0" "$(sed -n '/^raise_docker_mem_at_cold_start()/,/^}/p' scripts/local-autopilot.sh | grep -q 'cold_start_mem_write_decision' && echo 0 || echo 1)"
t "F19-A: the settings file is backed up before the write" "0" "$(sed -n '/^raise_docker_mem_at_cold_start()/,/^}/p' scripts/local-autopilot.sh | grep -q 'tickvault-bak' && echo 0 || echo 1)"
t "F19-A: post-start VM memory is logged (proof channel)" "0" "$(grep -q 'Docker VM memory after start' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F19-A: the flush-on-exit theory is labeled Assumed" "0" "$(grep -q 'Assumed' scripts/local-autopilot.sh && echo 0 || echo 1)"
# FIX 29 update: the hint now reads the SAME single source every operator
# message uses (docker_mem_advice_mib → docker_mem_target_mib underneath).
t "F19-A: the warm-Docker hint default is host-aware too (via the FIX 29 single source)" "0" "$(sed -n '/^hint_docker_vm_memory()/,/^}/p' scripts/local-autopilot.sh | grep -q 'docker_mem_advice_mib' && echo 0 || echo 1)"

# Item 7: guaranteed per-feed Telegram status pings
t "F19-7: OFF feed is announced, never silent" "⏸️ dhan: switched OFF for this run (expected in a lab/max-smoke session — not an error)" "$(feed_ping_text dhan false unknown false 0)"
# FIX 20 task 2: "data flowing" is claimed ONLY with ticks>0 — the ok
# verdict wording is state-based (ticks + market-open), never a false claim.
t "F20: ok + ticks flowing → data flowing" "✅ groww: connected, 1000 instruments subscribed, data flowing" "$(feed_ping_text groww true ok true 1000 42 1)"
t "F20: ok + 0 ticks + market closed → awaiting first tick" "🟢 groww: sidecar up, 768 subscribed — awaiting first tick (market closed)" "$(feed_ping_text groww true ok true 768 0 0)"
t "F20: ok + 0 ticks + market OPEN → honest no-ticks warning" "⚠️ groww: connected, 768 subscribed — NO ticks yet (investigating if it persists)" "$(feed_ping_text groww true ok true 768 0 1)"
t "F20: omitted tick args fail SAFE (never claims data flowing)" "0" "$(case "$(feed_ping_text groww true ok true 1000)" in *"data flowing"*) echo 1 ;; *) echo 0 ;; esac)"
t "F20: garbage ticks count renders as 0 (no false flowing)" "0" "$(case "$(feed_ping_text groww true ok true 1000 banana 1)" in *"data flowing"*) echo 1 ;; *) echo 0 ;; esac)"
t "F20: garbage market flag fails safe to closed wording" "🟢 groww: sidecar up, 10 subscribed — awaiting first tick (market closed)" "$(feed_ping_text groww true ok true 10 0 x)"
# FIX 27: market CLOSED must never claim "connected" — the 2026-07-05
# 17:56 IST live run proved the Dhan socket was DEFERRED (app log:
# "WebSocket boot connect deferred — outside market hours") while the
# ping said "✅ dhan: connected". State-based honest wording per feed.
t "F27: dhan closed → deferred wording (exact)" "🔷 dhan: 776 instruments ready — WebSocket connect deferred until market open (09:00 IST)" "$(feed_ping_text dhan true ok true 776 0 0)"
t "F27: dhan closed wording never says connected" "0" "$(case "$(feed_ping_text dhan true ok true 776 0 0)" in *connected*) echo 1 ;; *) echo 0 ;; esac)"
t "F27: dhan open + 0 ticks → honest no-ticks warning (says connected)" "⚠️ dhan: connected, 776 subscribed — NO ticks yet (investigating if it persists)" "$(feed_ping_text dhan true ok true 776 0 1)"
t "F27: dhan open + ticks → data flowing (unchanged)" "✅ dhan: connected, 776 instruments subscribed, data flowing" "$(feed_ping_text dhan true ok true 776 9 1)"
t "F27: groww closed → sidecar-up wording (exact)" "🟢 groww: sidecar up, 768 subscribed — awaiting first tick (market closed)" "$(feed_ping_text groww true ok true 768 0 0)"
t "F27: groww closed wording never says connected" "0" "$(case "$(feed_ping_text groww true ok true 768 0 0)" in *connected*) echo 1 ;; *) echo 0 ;; esac)"
t "F27: groww open + ticks → data flowing (unchanged)" "✅ groww: connected, 768 instruments subscribed, data flowing" "$(feed_ping_text groww true ok true 768 12 1)"
t "F27: unknown future feed closed → neutral no-connected line" "⏳ newfeed: 5 subscribed — awaiting first tick (market closed)" "$(feed_ping_text newfeed true ok true 5 0 0)"
t "F27: unknown future feed closed never says connected" "0" "$(case "$(feed_ping_text newfeed true ok true 5 0 0)" in *connected*) echo 1 ;; *) echo 0 ;; esac)"
t "F20: health rows carry the ticks column" "0" "$(grep -q '"ticks_total"' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F20: pinger passes ticks + market flag to the wording fn" "0" "$(grep -q 'feed_ping_text "\$name" "\$enabled" "\$verdict" "\$connected" "\$sub" "\$ticks" "\$mkt"' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F19-7: unknown verdict but connected → awaiting ticks" "✅ groww: connected, 1000 instruments subscribed, awaiting ticks" "$(feed_ping_text groww true unknown true 1000)"
t "F19-7: down verdict → failed wording" "0" "$(feed_ping_text groww true down false 0 | grep -q 'NOT connected (failed: down)' && echo 0 || echo 1)"
t "F19-7: degraded verdict named" "0" "$(feed_ping_text groww true degraded true 42 | grep -q 'DEGRADED' && echo 0 || echo 1)"
t "F19-7: enabled but not yet connected → still connecting" "0" "$(feed_ping_text dhan true unknown false 0 | grep -q 'still connecting' && echo 0 || echo 1)"
t "F19-7: garbage subscribed count renders as 0" "0" "$(feed_ping_text groww true ok true banana | grep -q ' 0 subscribed' && echo 0 || echo 1)"
t "F19-7: settled when every enabled row connected" "0" "$(feed_rows_settled 'dhan|false|disabled|false|0
groww|true|ok|true|1000' && echo 0 || echo 1)"
t "F19-7: NOT settled while an enabled row is unknown+disconnected" "1" "$(feed_rows_settled 'groww|true|unknown|false|0' && echo 0 || echo 1)"
t "F19-7: settled when the unknown row is disabled" "0" "$(feed_rows_settled 'dhan|false|unknown|false|0' && echo 0 || echo 1)"
t "F19-7: empty rows are NOT settled (keep polling)" "1" "$(feed_rows_settled "" && echo 0 || echo 1)"
t "F19-7: enabled row with a real verdict is settled even if disconnected" "0" "$(feed_rows_settled 'groww|true|down|false|0' && echo 0 || echo 1)"
# wiring scans: dispatch entry + one spawn per start-complete Telegram
t "F19-7: feed-pings subcommand dispatched" "0" "$(grep -q 'feed-pings) send_feed_status_pings' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F19-7: all 4 start paths spawn the pinger (4th = FIX 30 auto-probe resume)" "4" "$(grep -c 'spawn_feed_status_pings # FIX 19 item 7' scripts/local-autopilot.sh || true)"
t "F19-7: the pinger reads the public feeds-health endpoint" "0" "$(grep -q 'api/feeds/health' scripts/local-autopilot.sh && echo 0 || echo 1)"

# ── FIX 21: post-open per-class tick coverage + pre-open socket probe ───────
t "F21: class label idx_i → indices" "indices" "$(tick_class_label idx_i)"
t "F21: class label nse_eq → spots" "spots" "$(tick_class_label nse_eq)"
t "F21: class label nse_fno → FNO" "FNO" "$(tick_class_label nse_fno)"
t "F21: unknown class label passes through" "weird" "$(tick_class_label weird)"
t "F21: every class flowing → ✅ verdict" \
  "✅ groww ticks by class since open: indices 120, spots 4500 — every class flowing" \
  "$(tick_class_verdict groww "idx_i nse_eq" "$(printf 'idx_i|120\nnse_eq|4500')")"
t "F21: one silent class → 🆘 names the dead class, keeps the live one" \
  "🆘 groww: SPOTS silent since open (0 ticks) while indices 120 flowing — mapping or server-side filter issue" \
  "$(tick_class_verdict groww "idx_i nse_eq" "idx_i|120")"
t "F21: all classes silent → feed-wide 🆘" \
  "🆘 groww: NO ticks in ANY class since open (INDICES, SPOTS all at 0) — feed-wide problem; check the feed status messages" \
  "$(tick_class_verdict groww "idx_i nse_eq" "")"
t "F21: missing watch file → honest could-not-verify (never ✅)" \
  "0" "$(case "$(tick_class_verdict groww "" "idx_i|120")" in "⚠️"*"NOT performed"*) echo 0 ;; *) echo 1 ;; esac)"
t "F21: garbage count row fails safe to 0 (dead)" \
  "0" "$(case "$(tick_class_verdict groww "idx_i" "idx_i|banana")" in "🆘"*) echo 0 ;; *) echo 1 ;; esac)"
t "F21: QuestDB dataset parses to segment|count rows" \
  "$(printf 'idx_i|120\nnse_eq|4500')" \
  "$(printf '%s' '{"columns":[{"name":"segment"},{"name":"count"}],"dataset":[["IDX_I",120],["NSE_EQ",4500]]}' | tick_class_counts_parse)"
t "F21: QuestDB error body → rc 1" \
  "1" "$(printf '%s' '{"error":"table does not exist","query":"x"}' | tick_class_counts_parse >/dev/null && echo 0 || echo 1)"
t "F21: non-JSON body → rc 1" \
  "1" "$(printf '<html>proxy error</html>' | tick_class_counts_parse >/dev/null && echo 0 || echo 1)"
cat >"$TMP/watch-ok.json" <<'EOF'
{"entries":[{"kind":"index_value","symbol":"NIFTY"},{"kind":"ltp","symbol":"RELIANCE"}]}
EOF
cat >"$TMP/watch-idx-only.json" <<'EOF'
{"entries":[{"kind":"index_value","symbol":"NIFTY"}]}
EOF
t "F21: watch file with indices + spots → both classes expected" "idx_i nse_eq" "$(watch_expected_classes "$TMP/watch-ok.json")"
t "F21: indices-only watch file → idx_i only" "idx_i" "$(watch_expected_classes "$TMP/watch-idx-only.json")"
t "F21: missing watch file → rc 1" "1" "$(watch_expected_classes "$TMP/nope.json" >/dev/null 2>&1 && echo 0 || echo 1)"
t "F21: tick-class check due at 09:20 IST" "0" "$(tick_class_check_due 33600 && echo 0 || echo 1)"
t "F21: tick-class check NOT due at 09:19:59" "1" "$(tick_class_check_due 33599 && echo 0 || echo 1)"
t "F21: garbage secs → not due" "1" "$(tick_class_check_due banana && echo 0 || echo 1)"
# addendum: pre-open socket reachability probe
t "F21: socket reachable → ✅ verdict" \
  "✅ groww socket reachable (socket-api.groww.in:443) — feed should connect at open" \
  "$(socket_probe_verdict 0)"
t "F21: socket unreachable → 🆘 names the Sunday timeout signature" \
  "0" "$(case "$(socket_probe_verdict 1)" in "🆘 groww socket UNREACHABLE at 09:05"*"not an account/cap issue"*) echo 0 ;; *) echo 1 ;; esac)"
t "F21: socket probe due at 09:05 IST" "0" "$(socket_probe_due 32700 && echo 0 || echo 1)"
t "F21: socket probe NOT due at 09:04:59" "1" "$(socket_probe_due 32699 && echo 0 || echo 1)"
t "F21: garbage secs → probe not due" "1" "$(socket_probe_due x && echo 0 || echo 1)"
# wiring pins: monitor loop fires both checks once/day; dispatch entries exist
t "F21: monitor loop fires the tick-class check" "0" "$(grep -q 'run_tick_class_check >>"\$(current_log)"' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F21: monitor loop fires the socket probe" "0" "$(grep -q 'run_groww_socket_probe >>"\$(current_log)"' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F21: both checks are trading-day gated" "0" "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'is_trading_day_now' && echo 0 || echo 1)"
t "F21: tick-class-check subcommand dispatched" "0" "$(grep -q 'tick-class-check) run_tick_class_check' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F21: socket-probe subcommand dispatched" "0" "$(grep -q 'socket-probe) run_groww_socket_probe' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F21: socket probe runner is timeout-bounded" "0" "$(sed -n '/^run_groww_socket_probe()/,/^}/p' scripts/local-autopilot.sh | grep -q -- '-w 5' && echo 0 || echo 1)"

# ── FIX 22: per-connection subscribe receipts + fleet summary ───────────────
t "F22: streaming conn → ✅ with real tick proof" \
  "c03 ✅ streaming (1000 subscribed, 42 ticks)" "$(conn_receipt_token 03 streaming 1000 42 0)"
t "F22: subscribed-only conn is NEVER faked as ✅" \
  "c04 loaded 1000 — connect UNVERIFIED (no tick yet)" "$(conn_receipt_token 04 subscribed 1000 0 0)"
t "F22: no status + NoServersError cycling → the known reason class" \
  "c05 retrying: connect timeout (server not answering)" "$(conn_receipt_token 05 - 0 0 1)"
t "F22: no status, no NoServers hint → plain retrying" \
  "c05 retrying (no status yet)" "$(conn_receipt_token 05 - 0 0 0)"
t "F22: garbage counts fail safe to 0" \
  "c00 ✅ streaming (0 subscribed, 0 ticks)" "$(conn_receipt_token 00 streaming banana pear 0)"
TOKENS_SMALL="$(printf 'c00 ✅ streaming (10 subscribed, 5 ticks)\nc01 loaded 10 — connect UNVERIFIED (no tick yet)')"
t "F22: small rung → ONE receipt message, no part label" \
  "🪜 rung 2 receipts: c00 ✅ streaming (10 subscribed, 5 ticks) | c01 loaded 10 — connect UNVERIFIED (no tick yet)" \
  "$(rung_receipt_lines 2 "$TOKENS_SMALL")"
TOKENS_BIG="$(for i in 0 1 2 3 4 5; do echo "c0$i ✅ streaming (10 subscribed, 5 ticks)"; done)"
t "F22: rung larger than the chunk splits into parts" "3" \
  "$(rung_receipt_lines 6 "$TOKENS_BIG" 2 | grep -c '🪜 rung 6 receipts (part ')"
t "F22: chunked parts carry every token exactly once" "6" \
  "$(rung_receipt_lines 6 "$TOKENS_BIG" 2 | grep -o 'c0[0-9]' | sort -u | wc -l | tr -d ' ')"
t "F22: empty tokens → honest receipts-unavailable line (never silent)" \
  "⚠️ rung 5: no connection status found — per-connection receipts unavailable (NOT claiming OK)" \
  "$(rung_receipt_lines 5 "")"
t "F22: garbage chunk falls back to the 25 default (single message)" "1" \
  "$(rung_receipt_lines 2 "$TOKENS_SMALL" banana | wc -l | tr -d ' ')"
t "F22: fleet all streaming → ✅ summary" \
  "✅ fleet at ceiling 10/10: 10 streaming, 0 loaded-unverified, 0 retrying — 10000 instruments subscribed across the fleet" \
  "$(fleet_summary_text 10 10 10 0 0 10000)"
t "F22: mixed fleet → ⚠️ (never fake ✅)" \
  "⚠️ fleet at ceiling 10/10: 8 streaming, 1 loaded-unverified, 1 retrying — 9000 instruments subscribed across the fleet" \
  "$(fleet_summary_text 10 10 8 1 1 9000)"
t "F22: zero streaming → 🆘 summary" \
  "🆘 fleet at ceiling 10/10: 0 streaming, 0 loaded-unverified, 10 retrying — 0 instruments subscribed across the fleet" \
  "$(fleet_summary_text 10 10 0 0 10 0)"
t "F22: garbage summary inputs fail safe to 0" \
  "🆘 fleet at ceiling 0/0: 0 streaming, 0 loaded-unverified, 0 retrying — 0 instruments subscribed across the fleet" \
  "$(fleet_summary_text x y z '' - '')"
# wiring pins
t "F22: probe path spawns the rung-receipt watcher" "1" "$(grep -c 'spawn_rung_receipt_watcher # FIX 22' scripts/local-autopilot.sh || true)"
t "F22: rung-receipts subcommand dispatched" "0" "$(grep -q 'rung-receipts) run_rung_receipt_watcher' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F22: watcher reads the real per-conn status files" "0" "$(sed -n '/^conn_receipts()/,/^}/p' scripts/local-autopilot.sh | grep -q 'groww-status.json' && echo 0 || echo 1)"
t "F22: watcher self-terminates on a duration bound" "0" "$(sed -n '/^run_rung_receipt_watcher()/,/^}/p' scripts/local-autopilot.sh | grep -q 'elapsed.*duration' && echo 0 || echo 1)"
t "F22: NoServers hint scans the log for the known signature" "0" "$(sed -n '/^noservers_recent()/,/^}/p' scripts/local-autopilot.sh | grep -q 'NoServersError' && echo 0 || echo 1)"

# ── FIX 23: stray sweep decision + log-storm guard + load snapshot ─────────
t "F23: extras beyond expected → the OLDEST pids are killed" \
  "$(printf '100\n200')" "$(stray_kill_pids 1 100 200 300)"
t "F23: expected 0 → every stray pid is killed" \
  "$(printf '100\n200\n300')" "$(stray_kill_pids 0 100 200 300)"
t "F23: count within expected → nothing killed" "" "$(stray_kill_pids 3 100 200 300)"
t "F23: no pids at all → nothing killed" "" "$(stray_kill_pids 0)"
t "F23: garbage expected fails SAFE (kill nothing)" "" "$(stray_kill_pids banana 100 200)"
t "F23: garbage pids are ignored, numeric ones decided" "100" "$(stray_kill_pids 1 100 banana 200)"
t "F23: unsorted input still keeps the newest" "$(printf '5\n90')" "$(stray_kill_pids 1 90 300 5)"
t "F23: pids_except drops only the keeper" "$(printf '100\n300')" "$(pids_except 200 100 200 300)"
t "F23: pids_except with empty keeper keeps all" "$(printf '100\n200')" "$(pids_except '' 100 200)"
t "F23: log growth over the cap → warn" "0" "$(log_growth_exceeded 100 350 200 && echo 0 || echo 1)"
t "F23: log growth at the cap → no warn" "1" "$(log_growth_exceeded 100 300 200 && echo 0 || echo 1)"
t "F23: log shrank (rotation) → no warn" "1" "$(log_growth_exceeded 300 100 200 && echo 0 || echo 1)"
t "F23: garbage sizes fail SAFE (no warn)" "1" "$(log_growth_exceeded banana 300 200 && echo 0 || echo 1)"
t "F23: empty sizes fail SAFE (no warn)" "1" "$(log_growth_exceeded '' '' 200 && echo 0 || echo 1)"
t "F23: load snapshot line renders all three parts" \
  "load: cpu 42% total, ~12.3 GB in use, top: tickvault 25%, questdb 9%, python3 3%" \
  "$(load_snapshot_text 42 12.3 "tickvault 25%, questdb 9%, python3 3%")"
t "F23: missing probe values render ? (never fabricated)" \
  "load: cpu ?% total, ~? GB in use, top: ?" "$(load_snapshot_text '' '' '')"
# wiring pins
t "F23: probe outcome path sweeps strays" "1" "$(grep -c 'sweep_stray_lab_processes 0 # FIX 23: post-probe stray sweep' scripts/local-autopilot.sh || true)"
t "F23: manual stop sweeps strays" "1" "$(grep -c 'sweep_stray_lab_processes 0 # FIX 23: a Stop leaves NO leftover lab process behind' scripts/local-autopilot.sh || true)"
t "F23: both adopt paths sweep non-children strays" "2" "$(grep -c 'sweep_stray_lab_processes 0 "\$' scripts/local-autopilot.sh || true)"
t "F23: adopted app's own sidecars are excluded (parent match)" "0" "$(sed -n '/^stray_sidecar_candidates()/,/^}/p' scripts/local-autopilot.sh | grep -q 'ppid' && echo 0 || echo 1)"
t "F23: telegram fires only when something was swept" "0" "$(sed -n '/^sweep_stray_lab_processes()/,/^}/p' scripts/local-autopilot.sh | grep -q 'caff_killed" -gt 0' && echo 0 || echo 1)"
t "F23: a single caffeinate is never touched" "0" "$(sed -n '/^sweep_stray_lab_processes()/,/^}/p' scripts/local-autopilot.sh | grep -q 'caff_total:-0}" -gt 1' && echo 0 || echo 1)"
t "F23: monitor loop carries the log-storm guard" "0" "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'log_growth_exceeded' && echo 0 || echo 1)"
t "F23: monitor loop logs the load snapshot" "0" "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'load_snapshot_line' && echo 0 || echo 1)"
t "F23: cmd_status shows the load snapshot" "0" "$(sed -n '/^cmd_status()/,/^}/p' scripts/local-autopilot.sh | grep -q 'load_snapshot_line' && echo 0 || echo 1)"
t "F23: sweep subcommand dispatched" "0" "$(grep -q 'sweep) sweep_stray_lab_processes' scripts/local-autopilot.sh && echo 0 || echo 1)"

# ── FIX 24: rust shadow client status → text ────────────────────────────────
t "F24: disabled shadow renders OFF" "rust shadow: OFF" "$(rust_shadow_status_text 0 0 0)"
t "F24: capturing claimed ONLY with real lines" \
  "🦀 rust shadow: capturing (1234 lines today)" "$(rust_shadow_status_text 1 1234 0)"
t "F24: ON + empty + native errors → honest retrying" \
  "🦀 rust shadow: ON but retrying (no capture yet — connection problems in the log)" \
  "$(rust_shadow_status_text 1 0 1)"
t "F24: ON + empty + quiet log → awaiting first tick" \
  "🦀 rust shadow: ON, no capture yet (awaiting first tick)" "$(rust_shadow_status_text 1 0 0)"
t "F24: garbage line count fails safe (never fake capturing)" \
  "🦀 rust shadow: ON, no capture yet (awaiting first tick)" "$(rust_shadow_status_text 1 banana 0)"
t "F24: garbage enabled flag renders OFF (fail safe)" "rust shadow: OFF" "$(rust_shadow_status_text x 100 0)"
# wiring pins
t "F24: local overlay enables the shadow client" "1" "$(grep -c '^groww_native_shadow = true' config/local.toml || true)"
t "F24: base default stays OFF (§35 lock untouched)" "1" "$(grep -c '^groww_native_shadow = false' config/base.toml || true)"
t "F24: feed pings carry the shadow status line" "0" "$(sed -n '/^send_feed_status_pings()/,/^}/p' scripts/local-autopilot.sh | grep -q 'rust_shadow_status_line' && echo 0 || echo 1)"
t "F24: cmd_status carries the shadow status line" "0" "$(sed -n '/^cmd_status()/,/^}/p' scripts/local-autopilot.sh | grep -q 'rust_shadow_status_line' && echo 0 || echo 1)"
t "F24: status reads the real shadow NDJSON capture file" "0" "$(grep -q 'rust-live-ticks.ndjson' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F24: status checks the GROWW-NATIVE log signature" "0" "$(sed -n '/^rust_shadow_status_line()/,/^}/p' scripts/local-autopilot.sh | grep -q 'GROWW-NATIVE-' && echo 0 || echo 1)"
t "F24: enabled flag resolves local.toml over base.toml" "0" "$(sed -n '/^rust_shadow_enabled()/,/^}/p' scripts/local-autopilot.sh | grep -q 'config/local.toml' && echo 0 || echo 1)"

# ── FIX 25: disarmed probe marker + narrowed stray-sweep candidates ─────────
# 1) probe marker: a blank or comment-only marker must NEVER fire the probe
#    (fresh-clone trap: a committed dated marker + gitignored burned stamp
#    would re-fire the ~50-min 100-conn probe on every fresh clone).
t "F25: blank marker line → skip (never probe)" "skip" \
  "$(probe_once_decision "$(probe_marker_date "")" 2026-07-05 0)"
t "F25: comment-only marker line → skip (never probe)" "skip" \
  "$(probe_once_decision "$(probe_marker_date "# disarmed 2026-07-05 — attempt 2 complete")" 2026-07-05 0)"
t "F25: comment marker attempt parse fails safe to 1" "1" \
  "$(probe_marker_attempt "# disarmed 2026-07-05 — attempt 2 complete")"
t "F25: a real dated marker still fires (disarm did not break arming)" "run" \
  "$(probe_once_decision 2026-07-05 2026-07-05 0)"
t "F25/F30: committed marker is DISARMED (comment) or a well-formed dated arm line — never garbage" "0" \
  "$(head -1 deploy/local/probe-once.date | grep -Eq '^#|^[0-9]{4}-[0-9]{2}-[0-9]{2}( [0-9]+)?$' && echo 0 || echo 1)"
t "F25: marker file documents the re-arm/disarm procedure" "0" \
  "$(grep -q 're-arm deliberately' deploy/local/probe-once.date && echo 0 || echo 1)"
# 2) stray-sweep candidates: only REAL interpreter invocations qualify.
t "F25: python3 + script path IS a sidecar candidate" "0" \
  "$(is_sidecar_cmdline 'python3 scripts/groww-sidecar/groww_sidecar.py --conn 3' && echo 0 || echo 1)"
t "F25: venv python IS a sidecar candidate" "0" \
  "$(is_sidecar_cmdline '/Users/p/.venv/bin/python scripts/groww-sidecar/groww_sidecar.py' && echo 0 || echo 1)"
t "F25: capitalized Python framework binary IS a candidate" "0" \
  "$(is_sidecar_cmdline '/System/Library/Python.app/Contents/MacOS/Python groww_sidecar.py' && echo 0 || echo 1)"
t "F25: vim editing the script is NOT a candidate" "1" \
  "$(is_sidecar_cmdline 'vim groww_sidecar.py' && echo 0 || echo 1)"
t "F25: less paging the script is NOT a candidate" "1" \
  "$(is_sidecar_cmdline 'less scripts/groww-sidecar/groww_sidecar.py' && echo 0 || echo 1)"
t "F25: empty cmdline fails SAFE (not a candidate)" "1" \
  "$(is_sidecar_cmdline '' && echo 0 || echo 1)"
t "F25: candidates fn requires the real-cmdline classifier" "0" \
  "$(sed -n '/^stray_sidecar_candidates()/,/^}/p' scripts/local-autopilot.sh | grep -q 'is_sidecar_cmdline' && echo 0 || echo 1)"

# ── FIX 26: auto-open the Live Board on Run click when the build serves it ──
t "F26: route answers 200 → open" "open" "$(board_open_decision 200)"
t "F26: 404 (older build) → skip" "skip" "$(board_open_decision 404)"
t "F26: 000 (timeout/no answer) → skip" "skip" "$(board_open_decision 000)"
t "F26: 500 → skip (never open a broken page)" "skip" "$(board_open_decision 500)"
t "F26: empty code fails safe to skip" "skip" "$(board_open_decision '')"
t "F26: garbage code fails safe to skip" "skip" "$(board_open_decision banana)"
# wiring pins
t "F26: manual-start path opens the board after health confirmation" "0" \
  "$(sed -n '/^cmd_start()/,/^}/p' scripts/local-autopilot.sh | grep -q 'maybe_open_live_board' && echo 0 || echo 1)"
t "F26: both adopt paths open the board" "2" \
  "$(sed -n '/^start_app_inner()/,/^}/p' scripts/local-autopilot.sh | grep -c 'maybe_open_live_board' || true)"
t "F26: probe is timeout-bounded" "0" \
  "$(sed -n '/^maybe_open_live_board()/,/^}/p' scripts/local-autopilot.sh | grep -q -- '--max-time 3' && echo 0 || echo 1)"
t "F26: opener consults the pure decision (never opens on non-200)" "0" \
  "$(sed -n '/^maybe_open_live_board()/,/^}/p' scripts/local-autopilot.sh | grep -q 'board_open_decision' && echo 0 || echo 1)"
t "F26: opener targets the /board route" "0" \
  "$(sed -n '/^maybe_open_live_board()/,/^}/p' scripts/local-autopilot.sh | grep -q '/board' && echo 0 || echo 1)"

# ── FIX 28: evening caffeinate re-arm guard (the 4h sleep-protection cliff) ─
# Decision boundaries: plenty of protection left → wait; inside the lead
# window with re-arms remaining → rearm; at the cap → expire (final
# Telegram); garbage → expire (fail loud, never a silent loop).
t "F28: hours of protection left → wait (no-op)" "wait" "$(caffeinate_guard_decision 10000 0 2 600)"
t "F28: one second above the lead window → still wait" "wait" "$(caffeinate_guard_decision 601 0 2 600)"
t "F28: lead boundary (600s left) → first re-arm" "rearm" "$(caffeinate_guard_decision 600 0 2 600)"
t "F28: near expiry, one re-arm used → second re-arm" "rearm" "$(caffeinate_guard_decision 300 1 2 600)"
t "F28: re-arm cap reached → expire (final Telegram)" "expire" "$(caffeinate_guard_decision 500 2 2 600)"
t "F28: past the cap → expire" "expire" "$(caffeinate_guard_decision 500 3 2 600)"
t "F28: zero cap → expire immediately (no extensions configured)" "expire" "$(caffeinate_guard_decision 500 0 0 600)"
t "F28: already at 0s but re-arms remain → rearm (never a silent cliff)" "rearm" "$(caffeinate_guard_decision 0 0 2 600)"
t "F28: lead defaults to 600s when omitted" "rearm" "$(caffeinate_guard_decision 600 0 2)"
t "F28: garbage secs_left → expire (fail loud, never loop)" "expire" "$(caffeinate_guard_decision banana 0 2 600)"
t "F28: empty rearms count → expire" "expire" "$(caffeinate_guard_decision 500 "" 2 600)"
t "F28: garbage max → expire" "expire" "$(caffeinate_guard_decision 500 0 x 600)"
t "F28: garbage lead → expire" "expire" "$(caffeinate_guard_decision 500 0 2 soon)"
# wiring pins
t "F28: guard loop consults the pure decision" "0" \
  "$(sed -n '/^cmd_caff_guard()/,/^}/p' scripts/local-autopilot.sh | grep -q 'caffeinate_guard_decision' && echo 0 || echo 1)"
t "F28: every disarm path also kills the guard (Stop always wins)" "0" \
  "$(sed -n '/^stop_caffeinate()/,/^}/p' scripts/local-autopilot.sh | grep -q 'stop_caffeinate_guard' && echo 0 || echo 1)"
t "F28: the evening no-monitor skip branch spawns the guard" "0" \
  "$(sed -n '/^maybe_spawn_manual_monitor()/,/^}/p' scripts/local-autopilot.sh | grep -q 'spawn_caffeinate_guard' && echo 0 || echo 1)"
t "F28: guard subcommand is dispatched" "0" \
  "$(grep -q '^caff-guard) cmd_caff_guard' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F28: cap defaults to 2 extensions (max ~12h total)" "0" \
  "$(grep -q 'CAFF_MAX_REARMS:-2' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F28: re-arm lead defaults to ~10 minutes before expiry" "0" \
  "$(grep -q 'CAFF_REARM_LEAD_SECS:-600' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F28: extension Telegram names the extension count + says press Stop" "0" \
  "$(sed -n '/^cmd_caff_guard()/,/^}/p' scripts/local-autopilot.sh | grep -q 'sleep protection extended another' && echo 0 || echo 1)"
t "F28: final-expiry Telegram says the Mac may sleep (no silent cliff)" "0" \
  "$(sed -n '/^cmd_caff_guard()/,/^}/p' scripts/local-autopilot.sh | grep -q 'sleep protection expired — Mac may sleep; press Stop or Start again' && echo 0 || echo 1)"
t "F28: guard pidfile lives under the autopilot state dir" "0" \
  "$(grep -q 'CAFF_GUARD_PIDFILE="\$AUTOPILOT_DIR/caffeinate-guard.pid"' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F28: re-arm updates the caffeinate pidfile (sweep + Stop track the new holder)" "0" \
  "$(sed -n '/^cmd_caff_guard()/,/^}/p' scripts/local-autopilot.sh | grep -q 'echo \$! >"\$CAFF_PIDFILE"' && echo 0 || echo 1)"

# ── FIX 29: ONE Docker memory number (Telegram == preference writer) ────────
# The settings-store writer computed a host-aware 12 GB while every
# Telegram said "raise it to 10 GB" — contradictory numbers for the same
# knob. docker_mem_advice_mib/gb is the single source both now read.
t "F29: advice mib — 48GB host → 12288" "12288" "$(docker_mem_advice_mib "" $((48 * 1024 * 1024 * 1024)))"
t "F29: advice mib — 16GB host → 10240" "10240" "$(docker_mem_advice_mib "" $((16 * 1024 * 1024 * 1024)))"
t "F29: advice mib — numeric override wins" "9000" "$(docker_mem_advice_mib 9000 $((48 * 1024 * 1024 * 1024)))"
t "F29: advice mib — garbage override falls back to host-aware" "12288" "$(docker_mem_advice_mib banana $((48 * 1024 * 1024 * 1024)))"
t "F29: advice gb — 48GB host → 12" "12" "$(docker_mem_advice_gb "" $((48 * 1024 * 1024 * 1024)))"
t "F29: advice gb — 16GB host → 10" "10" "$(docker_mem_advice_gb "" $((16 * 1024 * 1024 * 1024)))"
t "F29: advice gb rounds UP (9000 MiB → 9 GB)" "9" "$(docker_mem_advice_gb 9000 "")"
t "F29: advice gb — everything empty → conservative 10" "10" "$(docker_mem_advice_gb "" "")"
# wiring pins: no message may hardcode the old contradictory number, and
# every advice number must come from the single source.
t "F29: no operator message hardcodes 'to 10 GB' anymore" "0" "$(grep -c 'raise it to 10 GB' scripts/local-autopilot.sh || true)"
t "F29: OOM Telegram reads the single source" "0" \
  "$(grep 'OUT OF ITS MEMORY ALLOWANCE' scripts/local-autopilot.sh | grep -q 'docker_mem_advice_gb_now' && echo 0 || echo 1)"
t "F29: both VM-memory-check Telegrams read the single source" "2" \
  "$(sed -n '/^check_docker_vm_memory()/,/^}/p' scripts/local-autopilot.sh | grep -c 'docker_mem_advice_gb_now' || true)"
t "F29: scale-day skip Telegram reads the single source" "0" \
  "$(grep 'ladder is SKIPPED today' scripts/local-autopilot.sh | grep -q 'docker_mem_advice_gb_now' && echo 0 || echo 1)"
t "F29: messages say the manual slider is the reliable path (hint may be ignored)" "4" \
  "$(grep -c 'the automatic hint may be ignored by Docker' scripts/local-autopilot.sh || true)"
t "F29: the cold-start writer reads the single source too" "0" \
  "$(sed -n '/^raise_docker_mem_at_cold_start()/,/^}/p' scripts/local-autopilot.sh | grep -q 'docker_mem_advice_mib' && echo 0 || echo 1)"
t "F29: single source honors the TV_DOCKER_VM_MEM_MIB override (runtime wrapper)" "0" \
  "$(sed -n '/^docker_mem_advice_gb_now()/,/^}/p' scripts/local-autopilot.sh | grep -q 'TV_DOCKER_VM_MEM_MIB' && echo 0 || echo 1)"

# ── FIX 30: robot auto-fires the armed once-probe (10:00-13:30 IST window) ──
# Decision boundaries: the 08:55 robot fires the SAME one-shot ladder probe
# a manual Start click would, but only 10:00:00-13:29:59 IST (after the
# 09:15 open + ~09:20 per-class verdict; before 13:30 so the 75-min cap +
# resume land well before the 15:30 close).
t "F30: 09:59:59 armed unstamped idle → wait" "wait" "$(probe_auto_fire_decision 09:59:59 1 0 idle)"
t "F30: 10:00:00 → fire" "fire" "$(probe_auto_fire_decision 10:00:00 1 0 idle)"
t "F30: 13:29:59 → fire (last eligible second)" "fire" "$(probe_auto_fire_decision 13:29:59 1 0 idle)"
t "F30: 13:30:00 → skip (window missed)" "skip" "$(probe_auto_fire_decision 13:30:00 1 0 idle)"
t "F30: stamped → skip (once-only preserved)" "skip" "$(probe_auto_fire_decision 11:00:00 1 1 idle)"
t "F30: unarmed → skip (inert on every other day)" "skip" "$(probe_auto_fire_decision 11:00:00 0 0 idle)"
t "F30: unarmed before window → skip, NOT wait" "skip" "$(probe_auto_fire_decision 09:00:00 0 0 idle)"
t "F30: phase fired → skip (in-session double-fire guard)" "skip" "$(probe_auto_fire_decision 11:00:00 1 0 fired)"
t "F30: phase manual → skip (Stop always wins)" "skip" "$(probe_auto_fire_decision 11:00:00 1 0 manual)"
t "F30: empty phase fails safe → skip" "skip" "$(probe_auto_fire_decision 11:00:00 1 0 "")"
t "F30: garbage time fails safe → skip" "skip" "$(probe_auto_fire_decision 9x:00:00 1 0 idle)"
t "F30: empty time fails safe → skip" "skip" "$(probe_auto_fire_decision "" 1 0 idle)"
t "F30: numeric secs-of-day 36000 → fire" "fire" "$(probe_auto_fire_decision 36000 1 0 idle)"
t "F30: numeric secs-of-day 35999 → wait" "wait" "$(probe_auto_fire_decision 35999 1 0 idle)"
t "F30: numeric secs-of-day 48600 → skip" "skip" "$(probe_auto_fire_decision 48600 1 0 idle)"
# probe_marker_state — the marker + per-attempt stamp reads, factored so
# the robot gate and the scale-day precedence gate see the same truth.
mkdir -p "$TMP/f30"
printf '2099-01-11 3\n' >"$TMP/f30/marker"
t "F30: marker armed today, attempt 3, no stamp" "1 0 3" \
  "$(probe_marker_state "$TMP/f30/marker" 2099-01-11 "$TMP/f30")"
t "F30: date MISMATCH (Monday robot, Tuesday marker) → provably inert" "0 0 3" \
  "$(probe_marker_state "$TMP/f30/marker" 2099-01-10 "$TMP/f30")"
touch "$TMP/f30/probe-once.done.2099-01-11.3"
t "F30: per-attempt stamp present → stamped" "1 1 3" \
  "$(probe_marker_state "$TMP/f30/marker" 2099-01-11 "$TMP/f30")"
printf '2099-02-11\n' >"$TMP/f30/marker2"
t "F30: bare-date marker = attempt 1" "1 0 1" \
  "$(probe_marker_state "$TMP/f30/marker2" 2099-02-11 "$TMP/f30")"
touch "$TMP/f30/probe-once.done.2099-02-11"
t "F30: legacy unsuffixed stamp counts for attempt 1" "1 1 1" \
  "$(probe_marker_state "$TMP/f30/marker2" 2099-02-11 "$TMP/f30")"
printf '# disarmed — attempt done\n' >"$TMP/f30/marker3"
t "F30: disarmed comment marker → unarmed" "0 0 1" \
  "$(probe_marker_state "$TMP/f30/marker3" 2099-03-11 "$TMP/f30")"
t "F30: missing marker file → unarmed" "0 0 1" \
  "$(probe_marker_state "$TMP/f30/absent" 2099-03-11 "$TMP/f30")"
# End-to-end: a date-mismatched marker can never fire even mid-window.
read -r _f30a _f30s _f30n <<<"$(probe_marker_state "$TMP/f30/marker" 2099-01-10 "$TMP/f30")"
t "F30: mismatched marker feeds decision → skip at 11:00" "skip" \
  "$(probe_auto_fire_decision 11:00:00 "$_f30a" "$_f30s" idle)"
# Wiring pins: the robot loop consults the decision and fires the SAME
# one-shot flow the manual Start click uses; only cmd_run's main
# invocation arms it.
t "F30: cmd_run main monitor invocation arms auto-fire (exactly once)" "1" \
  "$(grep -c 'monitor_until_eod 1' scripts/local-autopilot.sh || true)"
t "F30: monitor loop consults probe_auto_fire_decision" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'probe_auto_fire_decision' && echo 0 || echo 1)"
t "F30: monitor loop reads marker state via probe_marker_state" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'probe_marker_state "\$PROBE_ONCE_MARKER"' && echo 0 || echo 1)"
t "F30: fire path invokes the SAME maybe_run_probe_once flow" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'if maybe_run_probe_once; then' && echo 0 || echo 1)"
t "F30: fire Telegram wording (pause + auto-resume promise)" "0" \
  "$(grep -qF 'starting — live session pauses up to 75 min, resumes automatically' scripts/local-autopilot.sh && echo 0 || echo 1)"
t "F30: resume path clears the probe's own internal-stop marker" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -qF 'rm -f "$MANUAL_STOP_MARKER"' && echo 0 || echo 1)"
t "F30: resume path re-arms caffeinate right after clearing the marker" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -A1 'rm -f "\$MANUAL_STOP_MARKER"' | grep -q 'start_caffeinate' && echo 0 || echo 1)"
t "F30: resume path sweeps a leaked scale overlay before relaunch" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'has_scale_overlay config/local.toml' && echo 0 || echo 1)"
t "F30: resume relaunches the normal session (peer-tolerant)" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'start_app_tolerate_peer "normal session resume after auto-fired probe"' && echo 0 || echo 1)"
t "F30: Stop pressed DURING the probe is honored (no auto-resume)" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'no auto-resume' && echo 0 || echo 1)"
t "F30: window-missed case logs honestly (marker stays armed)" "0" \
  "$(sed -n '/^monitor_until_eod()/,/^}/p' scripts/local-autopilot.sh | grep -q 'window missed' && echo 0 || echo 1)"
t "F30: manual-start monitor (cmd_monitor) does NOT arm auto-fire" "0" \
  "$(sed -n '/^cmd_monitor()/,/^}/p' scripts/local-autopilot.sh | grep -c 'monitor_until_eod 1' || true)"
t "F30: scale-day precedence — armed marker owns the fleet budget" "0" \
  "$(sed -n '/^cmd_run()/,/^}/p' scripts/local-autopilot.sh | grep -q 'the marker owns the fleet budget' && echo 0 || echo 1)"
t "F30: scale-day precedence gate reads the same marker state" "0" \
  "$(sed -n '/^cmd_run()/,/^}/p' scripts/local-autopilot.sh | grep -q 'probe_marker_state "\$PROBE_ONCE_MARKER" "\$TODAY"' && echo 0 || echo 1)"

# ── FIX 31: cold-start Docker VM memory auto-raise (quit→write→relaunch→verify)
# Decision boundaries — raise fires ONLY at a true cold start (app + database
# both down), below target, once per run.
t "F31: below target, cold, first attempt → raise" "raise" "$(fix31_raise_decision 8192 12288 0 0 0)"
t "F31: exactly at target → skip_at_target" "skip_at_target" "$(fix31_raise_decision 12288 12288 0 0 0)"
t "F31: above target → skip_at_target" "skip_at_target" "$(fix31_raise_decision 16384 12288 0 0 0)"
t "F31: app already running (warm) → skip_app_running" "skip_app_running" "$(fix31_raise_decision 8192 12288 1 0 0)"
t "F31: app dead + database container idling → raise_stop_qdb (2026-07-06 fix: stop it, then raise)" "raise_stop_qdb" "$(fix31_raise_decision 8192 12288 0 1 0)"
t "F31: second attempt same run → skip_attempted (single attempt)" "skip_attempted" "$(fix31_raise_decision 8192 12288 0 0 1)"
t "F31: attempted latch beats every other input" "skip_attempted" "$(fix31_raise_decision '' '' 1 1 1)"
t "F31: VM probe empty (daemon down → FIX 19 owns it) → skip_no_probe" "skip_no_probe" "$(fix31_raise_decision "" 12288 0 0 0)"
t "F31: VM probe garbage → skip_no_probe" "skip_no_probe" "$(fix31_raise_decision abc 12288 0 0 0)"
t "F31: garbage target → skip_no_target (fail-quiet)" "skip_no_target" "$(fix31_raise_decision 8192 "" 0 0 0)"
t "F31: garbage app flag fails SAFE to skip (never quit on uncertainty)" "skip_app_running" "$(fix31_raise_decision 8192 12288 banana 0 0)"
t "F31: garbage qdb flag fails SAFE to raise_stop_qdb (unknown = treat as running, bounded stop; app confirmed dead)" "raise_stop_qdb" "$(fix31_raise_decision 8192 12288 0 banana 0)"
# 2026-07-06 exam-incident matrix: the stop-container arm fires ONLY when the
# raise would really run — app running always wins; garbage/at-target never
# stop the container; the attempted latch still means single attempt.
t "F31: app running beats qdb idle → skip_app_running (never act while the app is up)" "skip_app_running" "$(fix31_raise_decision 8192 12288 1 1 0)"
t "F31: app dead + qdb idle + VM at target → skip_at_target (no stop, nothing to do)" "skip_at_target" "$(fix31_raise_decision 12288 12288 0 1 0)"
t "F31: app dead + qdb idle + attempted latch → skip_attempted (single attempt even for the stop arm)" "skip_attempted" "$(fix31_raise_decision 8192 12288 0 1 1)"
t "F31: app dead + qdb idle + garbage target → skip_no_target (never stop the container on garbage)" "skip_no_target" "$(fix31_raise_decision 8192 "" 0 1 0)"
t "F31: app dead + qdb idle + empty VM probe → skip_no_probe (daemon down owns it, no stop)" "skip_no_probe" "$(fix31_raise_decision "" 12288 0 1 0)"
t "F31: app dead + NO docker container → plain raise (no stop needed)" "raise" "$(fix31_raise_decision 8192 12288 0 0 0)"
# TV_DOCKER_VM_MEM_MIB override flows through the FIX 29 single source.
t "F31: override 16384 above a 12 GB VM → raise" "raise" "$(fix31_raise_decision 12288 "$(docker_mem_advice_mib 16384 '')" 0 0 0)"
t "F31: override already satisfied by the VM → skip_at_target" "skip_at_target" "$(fix31_raise_decision 16384 "$(docker_mem_advice_mib 16384 '')" 0 0 0)"

# Verification boundary: ok only at ≥ (target − 1 GB); garbage NEVER
# verifies (the 🧠 success Telegram fires only on proof — audit Rule 11).
t "F31 verify: full target bytes → ok" "ok" "$(fix31_verify_mem $((12288 * 1048576)) 12288)"
t "F31 verify: exactly target−1GB → ok" "ok" "$(fix31_verify_mem $((11264 * 1048576)) 12288)"
t "F31 verify: 1 byte under target−1GB → fail" "fail" "$(fix31_verify_mem $((11264 * 1048576 - 1)) 12288)"
t "F31 verify: old 8 GB VM after a clobbered write → fail" "fail" "$(fix31_verify_mem $((8192 * 1048576)) 12288)"
t "F31 verify: empty probe → fail (no false success)" "fail" "$(fix31_verify_mem "" 12288)"
t "F31 verify: garbage probe → fail" "fail" "$(fix31_verify_mem banana 12288)"
t "F31 verify: garbage target → fail" "fail" "$(fix31_verify_mem 123456789 "")"
t "F31 verify: tiny target floors at 1 MiB (no zero/negative floor)" "ok" "$(fix31_verify_mem $((2 * 1048576)) 100)"

# Quit-escalation ladder: polite osascript wait → pkill → give_up; gone = done.
t "F31 quit: app gone → done" "done" "$(fix31_quit_next 0 0 30 45)"
t "F31 quit: still up at 0s → wait" "wait" "$(fix31_quit_next 1 0 30 45)"
t "F31 quit: still up at 27s → wait" "wait" "$(fix31_quit_next 1 27 30 45)"
t "F31 quit: still up at 30s → pkill (polite window over)" "pkill" "$(fix31_quit_next 1 30 30 45)"
t "F31 quit: still up at 44s → pkill" "pkill" "$(fix31_quit_next 1 44 30 45)"
t "F31 quit: still up at 45s → give_up (hard window over)" "give_up" "$(fix31_quit_next 1 45 30 45)"
t "F31 quit: garbage waited-secs → give_up (never loop)" "give_up" "$(fix31_quit_next 1 banana 30 45)"
t "F31 quit: empty deadlines → give_up (never loop)" "give_up" "$(fix31_quit_next 1 10 "" "")"

# Post-stop verdict (2026-07-06 fail-closed hardening): ONLY a positive
# `exited*` probe counts as verifiably stopped. Empty (dk timeout on a
# wedged daemon / daemon error / no container) and every other state fail
# CLOSED to not_stopped — the raise aborts instead of quitting Docker over
# a container of unknown state (mirrors the decision side's unknown=running).
t "F31 stop-verdict: exited → stopped" "stopped" "$(fix31_stop_verdict "exited")"
t "F31 stop-verdict: exited + health suffix → stopped" "stopped" "$(fix31_stop_verdict "exited unhealthy")"
t "F31 stop-verdict: running healthy → not_stopped" "not_stopped" "$(fix31_stop_verdict "running healthy")"
t "F31 stop-verdict: EMPTY probe (dk timeout / wedged daemon) → not_stopped (fail closed)" "not_stopped" "$(fix31_stop_verdict "")"
t "F31 stop-verdict: restarting → not_stopped" "not_stopped" "$(fix31_stop_verdict "restarting")"
t "F31 stop-verdict: paused → not_stopped" "not_stopped" "$(fix31_stop_verdict "paused")"
t "F31 stop-verdict: removing → not_stopped" "not_stopped" "$(fix31_stop_verdict "removing")"
t "F31 stop-verdict: dead → not_stopped (fail closed on ambiguous state)" "not_stopped" "$(fix31_stop_verdict "dead")"
t "F31 stop-verdict: garbage → not_stopped" "not_stopped" "$(fix31_stop_verdict "banana")"

# Clobber forensics (the FIX 17 "ignored" post-mortem channel).
t "F31 forensics: file below target after relaunch → clobbered" "clobbered" "$(fix31_clobber_verdict 8192 12288)"
t "F31 forensics: file still at target → held" "held" "$(fix31_clobber_verdict 12288 12288)"
t "F31 forensics: file above target → held" "held" "$(fix31_clobber_verdict 16384 12288)"
t "F31 forensics: unreadable file value → unknown" "unknown" "$(fix31_clobber_verdict "" 12288)"
t "F31 forensics: garbage target → unknown" "unknown" "$(fix31_clobber_verdict 8192 banana)"

# Wiring pins — the executor's ORDER is the whole fix (FIX 14 wrote the file
# while Docker was running, then the exit-time flush clobbered it).
FIX31_BODY=$(sed -n '/^fix31_auto_raise_docker_mem()/,/^}/p' scripts/local-autopilot.sh)
t "F31 wiring: QUIT precedes the settings WRITE (the FIX 14 clobber lesson)" "ordered" \
  "$(printf '%s\n' "$FIX31_BODY" | awk '/quit app/{if(!q)q=NR} /docker_settings_set_mem/{if(!w)w=NR} END{print (q && w && q<w) ? "ordered" : "bad"}')"
t "F31 wiring: WRITE precedes the VERIFY (verify-after-relaunch)" "ordered" \
  "$(printf '%s\n' "$FIX31_BODY" | awk '/docker_settings_set_mem/{if(!w)w=NR} /fix31_verify_mem/{if(!v)v=NR} END{print (w && v && w<v) ? "ordered" : "bad"}')"
t "F31 wiring: single-attempt latch set inside the executor (exactly once)" "1" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -c 'FIX31_RAISE_ATTEMPTED=1' || true)"
t "F31 wiring: cold gate consults app_alive" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'app_alive' && echo 0 || echo 1)"
t "F31 wiring: cold gate consults the database container state" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'questdb_container_state' && echo 0 || echo 1)"
# 2026-07-06 fix: the app-dead + qdb-idle arm stops the container then raises.
t "F31 wiring: executor handles the raise_stop_qdb decision" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'raise_stop_qdb' && echo 0 || echo 1)"
t "F31 wiring: idle-container stop goes through dk with the bounded timeout" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'DOCKER_CMD_TIMEOUT_SECS="\$FIX31_QDB_STOP_TIMEOUT_SECS" dk stop tv-questdb' && echo 0 || echo 1)"
t "F31 wiring: container STOP is ordered BEFORE the osascript quit" "ordered" \
  "$(printf '%s\n' "$FIX31_BODY" | awk '/dk stop tv-questdb/{if(!s)s=NR} /quit app/{if(!q)q=NR} END{print (s && q && s<q) ? "ordered" : "bad"}')"
t "F31 wiring: post-stop re-check aborts the raise if the container is still running" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'did NOT stop within' && echo 0 || echo 1)"
# 2026-07-06 fail-closed pins: the executor must decide via the pure
# fix31_stop_verdict (positive `exited*` required — empty/unknown aborts),
# NOT via a `running*)` positive-match arm that fails OPEN on an empty probe.
t "F31 wiring: post-stop re-check decides via fix31_stop_verdict (fail closed)" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'fix31_stop_verdict' && echo 0 || echo 1)"
t "F31 wiring: the fail-OPEN 'running*)' positive-match arm is GONE from the post-stop section" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | sed -n '/dk stop tv-questdb/,$p' | grep -q 'running\*)' && echo 1 || echo 0)"
t "F31 wiring: stop-verdict check is ordered BEFORE the stopped-container Telegram" "ordered" \
  "$(printf '%s\n' "$FIX31_BODY" | awk '/fix31_stop_verdict/{if(!v)v=NR} /Stopped idle database container/{if(!t)t=NR} END{print (v && t && v<t) ? "ordered" : "bad"}')"
t "F31 wiring: stop-verdict check is ordered BEFORE the osascript quit" "ordered" \
  "$(printf '%s\n' "$FIX31_BODY" | awk '/fix31_stop_verdict/{if(!v)v=NR} /quit app/{if(!q)q=NR} END{print (v && q && v<q) ? "ordered" : "bad"}')"
t "F31 wiring: honest Telegram line for the stop-container arm" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'Stopped idle database container to apply the memory raise' && echo 0 || echo 1)"
t "F31 wiring: qdb stop timeout constant pinned at 60s (inside the 180s cycle budget)" "1" \
  "$(grep -c 'FIX31_QDB_STOP_TIMEOUT_SECS=60' scripts/local-autopilot.sh || true)"
t "F31 wiring: verify-FAIL fallback arm unchanged (clamp-and-proceed message kept)" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'falling back to clamp-and-proceed' && echo 0 || echo 1)"
t "F31 wiring: override reaches the executor via the FIX 29 single source" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'docker_mem_advice_mib "\${TV_DOCKER_VM_MEM_MIB' && echo 0 || echo 1)"
t "F31 wiring: success Telegram fires only on the PROVEN raise" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q '🧠 Docker memory raised' && echo 0 || echo 1)"
t "F31 wiring: verified failure logs the clobber forensics" "0" \
  "$(printf '%s\n' "$FIX31_BODY" | grep -q 'fix31_clobber_verdict' && echo 0 || echo 1)"
t "F31 wiring: whole cycle bounded to 3 minutes" "1" \
  "$(grep -c 'FIX31_CYCLE_BUDGET_SECS=180' scripts/local-autopilot.sh || true)"
t "F31 wiring: boot_chain_once runs fix31 (non-gating) before the memory check" "ordered" \
  "$(sed -n '/^boot_chain_once()/,/^}/p' scripts/local-autopilot.sh | awk '/ensure_docker/{if(!d)d=NR} /fix31_auto_raise_docker_mem \|\| true/{if(!f)f=NR} /check_docker_vm_memory/{if(!c)c=NR} END{print (d && f && c && d<f && f<c) ? "ordered" : "bad"}')"
t "F31 wiring: cmd_start runs fix31 (non-gating) before the memory check" "ordered" \
  "$(sed -n '/^cmd_start()/,/^}/p' scripts/local-autopilot.sh | awk '/ensure_docker/{if(!d)d=NR} /fix31_auto_raise_docker_mem \|\| true/{if(!f)f=NR} /check_docker_vm_memory/{if(!c)c=NR} END{print (d && f && c && d<f && f<c) ? "ordered" : "bad"}')"
t "F31 wiring: honest manual-slider fallback messages are KEPT (both branches)" "2" \
  "$(sed -n '/^check_docker_vm_memory()/,/^}/p' scripts/local-autopilot.sh | grep -c 'manual slider raise is the reliable fix' || true)"

echo
echo "local-autopilot pure-logic tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
