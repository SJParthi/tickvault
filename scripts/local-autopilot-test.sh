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
# Ratchets: the default path is clamp-and-proceed with NO Docker restart
# reachable — no osascript quit anywhere; 'open -a Docker' only at the
# ensure-docker start site (Docker not running at all ≠ a mid-run restart);
# the hint log promises next-start semantics; the clamp remains wired.
t "FIX 17: no Docker Desktop quit/restart anywhere" "0" "$(grep -c 'quit app' scripts/local-autopilot.sh || true)"
t "FIX 17: 'open -a Docker' only at the not-running start site" "1" "$(grep -c 'open -a Docker' scripts/local-autopilot.sh || true)"
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
# 15.5: the IntelliJ dev path carries the self-update (fetch + ff-only)
t "cmd_dev: self-update fetch present (FIX 15.5)" "1" "$(sed -n '/^cmd_dev()/,/^}/p' scripts/local-autopilot.sh | grep -c 'git fetch origin local-runtime')"
t "cmd_dev: ff-only merge present (FIX 15.5)" "1" "$(sed -n '/^cmd_dev()/,/^}/p' scripts/local-autopilot.sh | grep -c 'git merge --ff-only origin/local-runtime')"
# 15.7: the Telegram send curl is time-bounded
t "notify-telegram: curl has --max-time (FIX 15.7)" "1" "$(grep -c 'curl -s --max-time 10 --connect-timeout 5' scripts/notify-telegram.sh)"

echo
echo "local-autopilot pure-logic tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
