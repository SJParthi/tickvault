//! AUTO-GENERATED action command goldens for the operator-control port —
//! captured by RUNNING the legacy oracle's `lambda_handler`
//! (`deploy/aws/lambda/operator-control/handler.py`) with a stubbed
//! `_ssm_shell` (`scratchpad/w4-dump-actions.py`), NEVER hand-transcribed.
//! Byte-exact with the SSM command lists each action dispatches.

/// legacy: `lambda_handler wipe-questdb cmds` (handler.py:1126-1197) — captured from the RUNNING oracle.
pub const WIPE_QUESTDB_COMMANDS: [&str; 10] = [
    r#"set +e"#,
    r#"systemctl stop tickvault || true"#,
    r#"systemctl disable tickvault || true"#,
    r#"rm -rf /opt/tickvault/data/ws_wal /opt/tickvault/data/groww /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/instrument-cache 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"echo 'OK: feed capture/replay sources removed (ws_wal, groww, spill, dlq, instrument-cache)'"#,
    // 2026-08-01 (operator directive — pure Rust, nowhere the banned runtime):
    // this element WAS a 17-line embedded interpreter program dispatched via
    // SSM RunCommand to the prod box — i.e. the banned runtime EXECUTING in
    // production. Re-expressed as curl + POSIX shell with the SAME semantics:
    // same dynamic table discovery, same target predicate, same TRUNCATE per
    // target, same WIPE-TARGETS / TRUNCATED / TRUNCATE-FAILED stdout markers.
    // Table discovery uses QuestDB's CSV endpoint (/exp) instead of /exec so
    // the names parse with `tail`+`tr` and need no JSON reader on the box;
    // `curl --get --data-urlencode` performs the same URL encoding the old
    // program's quote() did. The independent WIPE-RESULT/WIPE-COMPLETE
    // verification tail (next elements) is unchanged and still proves the
    // counts actually reached zero — a botched wipe reports WIPE-PARTIAL,
    // never a silent success.
    r#"QDB='http://127.0.0.1:9000'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" | tail -n +2 | tr -d '"\r' | sed '/^$/d')
TARGETS=$(printf '%s\n' "$ALL" | awk '$0=="ticks" || index($0,"candles_")==1 || $0=="prev_day_ohlcv" || $0=="rest_spot_1m" || $0=="rest_option_chain_1m" || $0=="rest_option_contract_1m" || $0=="rest_fetch_audit"' | sort)
echo "WIPE-TARGETS $(printf '%s\n' "$TARGETS" | sed '/^$/d' | wc -l | tr -d ' ') $(printf '%s\n' "$TARGETS" | sed '/^$/d' | paste -sd' ' -)"
for t in $TARGETS; do
  if curl -fsS --max-time 30 --get --data-urlencode "query=TRUNCATE TABLE $t" "$QDB/exec" >/dev/null; then echo "TRUNCATED $t"; else echo "TRUNCATE-FAILED $t"; fi
done"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl start tickvault || true"#,
    r#"sleep 3; qc() { curl -fsS "http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20$1" 2>/dev/null | grep -o '\[\[[0-9]*' | grep -o '[0-9]*'; }; T=$(qc ticks); C=$(qc candles_1m); S=$(qc rest_spot_1m); O=$(qc rest_option_chain_1m); K=$(qc rest_option_contract_1m); A=$(qc rest_fetch_audit); echo "WIPE-RESULT ticks=${T:-?} candles_1m=${C:-?} rest_spot_1m=${S:-?} rest_option_chain_1m=${O:-?} rest_option_contract_1m=${K:-?} rest_fetch_audit=${A:-?}"; if [ "${T:-0}" = 0 ] && [ "${C:-0}" = 0 ] && [ "${S:-0}" = 0 ] && [ "${O:-0}" = 0 ] && [ "${K:-0}" = 0 ] && [ "${A:-0}" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain — inspect the counts + TRUNCATE-FAILED lines above'; fi"#,
];

/// legacy: `lambda_handler docker-reset cmds` (handler.py:1258-1306) — captured from the RUNNING oracle.
pub const DOCKER_RESET_COMMANDS: [&str; 18] = [
    r#"set +e"#,
    r#"systemctl stop tickvault || true"#,
    // ---- SEBI PRESERVE (added 2026-08-25) ----
    //
    // The sibling `wipe-questdb` action carefully allowlists ONLY market-data
    // tables, so the 5-year regulatory tables survive it. This action destroys
    // the whole `tv-questdb-data` volume, which takes them with it — with no
    // exclusion and nothing exported first. The typed-confirm guard and the
    // market-hours guard both exist and are unchanged; what did not exist was
    // any way to get the regulatory history back afterwards.
    //
    // Exports to a directory OUTSIDE the volume and outside every `rm -rf`
    // path in this action, so the data survives the reset with no credentials
    // and no S3 dependency.
    //
    // The abort rule is deliberately asymmetric. QuestDB unreachable =>
    // continue: this action exists partly to recover a wedged QuestDB, and
    // there is nothing to export from a server that cannot answer. A table
    // that EXISTS and fails to export => abort: that is data we could have
    // saved and chose not to.
    r#"QDB='http://127.0.0.1:9000'
OUT=/opt/tickvault/data/sebi-preserve/$(date -u +%Y%m%dT%H%M%SZ)
SEBI='instrument_lifecycle instrument_lifecycle_audit index_constituency order_audit'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" 2>/dev/null | tail -n +2 | tr -d '"\r' | sed '/^$/d')
if [ -z "$ALL" ]; then
  echo 'SEBI-PRESERVE-UNAVAILABLE: QuestDB did not answer — nothing could be exported. Proceeding, because this action is also the remedy for a wedged QuestDB.'
else
  mkdir -p "$OUT" || true
  FAIL=0
  for t in $SEBI; do
    if printf '%s\n' "$ALL" | grep -qx "$t"; then
      if curl -fsS --max-time 300 --get --data-urlencode "query=SELECT * FROM $t" "$QDB/exp" -o "$OUT/$t.csv"; then
        echo "SEBI-PRESERVED $t $(wc -c <"$OUT/$t.csv" | tr -d ' ') bytes -> $OUT/$t.csv"
      else
        echo "SEBI-PRESERVE-FAILED $t"; FAIL=1
      fi
    else
      echo "SEBI-ABSENT $t"
    fi
  done
  if [ "$FAIL" = 1 ]; then
    echo 'ABORTED: a 5-year SEBI table exists and could NOT be exported — refusing to destroy the database volume. Fix the export, or move the table aside deliberately, then re-run.'
    exit 1
  fi
fi"#,
    r#"docker ps -aq --filter volume=tv-questdb-data | xargs -r docker rm -f 2>/dev/null || true"#,
    r#"docker rm -f tv-questdb tv-loki tv-alloy 2>/dev/null || true"#,
    r#"cd /opt/tickvault/repo/deploy/docker || exit 0"#,
    r#"docker compose down -v --remove-orphans || true"#,
    r#"docker volume rm -f tv-questdb-data 2>/dev/null || true"#,
    r#"docker system prune -af --volumes || true"#,
    r#"if docker volume inspect tv-questdb-data >/dev/null 2>&1; then echo 'DOCKER-RESET-FAILED: tv-questdb-data still present (in-use) — NOT recreating to avoid re-attaching stale data. Holders:'; docker ps -a --filter volume=tv-questdb-data --format '{{.Names}} ({{.Status}})'; echo docker-reset-FAILED; exit 1; fi"#,
    r#"echo 'OK: tv-questdb-data removed'"#,
    r#"rm -rf /opt/tickvault/data/instrument-cache /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/ws_wal /opt/tickvault/data/groww 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"echo 'OK: host caches + feed capture/replay sources wiped (instrument-cache, spill, dlq, ws_wal, groww); logs preserved'"#,
    r#"bash /opt/tickvault/repo/scripts/ensure-questdb.sh || true"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl restart tickvault || true"#,
    r#"echo docker-reset-dispatched"#,
];

/// legacy: `lambda_handler docker-nuke-bare cmds` (handler.py:1338-1368) — captured from the RUNNING oracle.
pub const DOCKER_NUKE_BARE_COMMANDS: [&str; 12] = [
    r#"set +e"#,
    r#"systemctl stop tickvault || true"#,
    r#"systemctl disable tickvault || true"#,
    // ---- SEBI PRESERVE (added 2026-08-25) ----
    //
    // The sibling `wipe-questdb` action carefully allowlists ONLY market-data
    // tables, so the 5-year regulatory tables survive it. This action destroys
    // the whole `tv-questdb-data` volume, which takes them with it — with no
    // exclusion and nothing exported first. The typed-confirm guard and the
    // market-hours guard both exist and are unchanged; what did not exist was
    // any way to get the regulatory history back afterwards.
    //
    // Exports to a directory OUTSIDE the volume and outside every `rm -rf`
    // path in this action, so the data survives the reset with no credentials
    // and no S3 dependency.
    //
    // The abort rule is deliberately asymmetric. QuestDB unreachable =>
    // continue: this action exists partly to recover a wedged QuestDB, and
    // there is nothing to export from a server that cannot answer. A table
    // that EXISTS and fails to export => abort: that is data we could have
    // saved and chose not to.
    r#"QDB='http://127.0.0.1:9000'
OUT=/opt/tickvault/data/sebi-preserve/$(date -u +%Y%m%dT%H%M%SZ)
SEBI='instrument_lifecycle instrument_lifecycle_audit index_constituency order_audit'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" 2>/dev/null | tail -n +2 | tr -d '"\r' | sed '/^$/d')
if [ -z "$ALL" ]; then
  echo 'SEBI-PRESERVE-UNAVAILABLE: QuestDB did not answer — nothing could be exported. Proceeding, because this action is also the remedy for a wedged QuestDB.'
else
  mkdir -p "$OUT" || true
  FAIL=0
  for t in $SEBI; do
    if printf '%s\n' "$ALL" | grep -qx "$t"; then
      if curl -fsS --max-time 300 --get --data-urlencode "query=SELECT * FROM $t" "$QDB/exp" -o "$OUT/$t.csv"; then
        echo "SEBI-PRESERVED $t $(wc -c <"$OUT/$t.csv" | tr -d ' ') bytes -> $OUT/$t.csv"
      else
        echo "SEBI-PRESERVE-FAILED $t"; FAIL=1
      fi
    else
      echo "SEBI-ABSENT $t"
    fi
  done
  if [ "$FAIL" = 1 ]; then
    echo 'ABORTED: a 5-year SEBI table exists and could NOT be exported — refusing to destroy the database volume. Fix the export, or move the table aside deliberately, then re-run.'
    exit 1
  fi
fi"#,
    r#"docker ps -aq | xargs -r docker rm -f 2>/dev/null || true"#,
    r#"docker images -aq | xargs -r docker rmi -f 2>/dev/null || true"#,
    r#"docker volume ls -q | xargs -r docker volume rm -f 2>/dev/null || true"#,
    r#"docker system prune -af --volumes 2>/dev/null || true"#,
    r#"rm -rf /opt/tickvault/data/instrument-cache /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/ws_wal /opt/tickvault/data/groww 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"C=$(docker ps -aq 2>/dev/null | wc -l | tr -d ' '); I=$(docker images -aq 2>/dev/null | wc -l | tr -d ' '); V=$(docker volume ls -q 2>/dev/null | wc -l | tr -d ' '); echo "BARE-NUKE-RESULT containers=$C images=$I volumes=$V"; if [ "$C" = 0 ] && [ "$I" = 0 ] && [ "$V" = 0 ]; then echo bare-nuke-complete; else echo 'bare-nuke-PARTIAL: something is still present (likely in-use)'; fi"#,
    // 2026-08-20 INCIDENT FIX — the auto-start guarantee. This action used to
    // `disable` tickvault (line 65 above) and NEVER re-enable it. A disabled
    // unit does NOT auto-start at the next 08:30 IST boot, AND
    // `scripts/aws-autopilot.sh` reads a disabled unit as an INTENTIONAL
    // kill-switch and REFUSES to self-heal it — so one bare-nuke silently cost
    // an entire trading day (2026-08-20: box up 08:30:42, app dead, QuestDB
    // gone, four alarms firing, discovered only because the operator asked).
    //
    // Its two sibling destructive actions both already restore: WIPE_QUESTDB
    // does `enable`+`start`, DOCKER_RESET does `enable`. This one was the odd
    // man out. The INTENTIONAL kill-switch stays the separate `stop-app`
    // action, so aws-autopilot.sh's `disabled == operator meant it` semantics
    // remain correct. Placed last so the BARE-NUKE-RESULT verification above
    // still reports the true post-nuke counts.
    r#"systemctl enable tickvault || true"#,
];

/// legacy: `lambda_handler logs cmds` (handler.py:1414-1424) — captured from the RUNNING oracle.
pub const LOGS_COMMANDS: [&str; 7] = [
    r#"set +e"#,
    r#"echo ERR_BEGIN"#,
    r#"journalctl -u tickvault -p err -n 40 --no-pager 2>/dev/null | tail -40 || true"#,
    r#"echo ERR_END"#,
    r#"echo APP_BEGIN"#,
    r#"journalctl -u tickvault -n 40 --no-pager 2>/dev/null | tail -40 || true"#,
    r#"echo APP_END"#,
];
