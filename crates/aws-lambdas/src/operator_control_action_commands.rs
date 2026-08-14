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
    r#"sleep 3; qc() { curl -fsS "http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20$1" 2>/dev/null | grep -o '\[\[[0-9]*' | grep -o '[0-9]*'; }; T=$(qc ticks); C=$(qc candles_1m); S=$(qc spot_1m_rest); O=$(qc option_chain_1m); K=$(qc option_contract_1m_rest); A=$(qc rest_fetch_audit); echo "WIPE-RESULT ticks=${T:-?} candles_1m=${C:-?} spot_1m_rest=${S:-?} option_chain_1m=${O:-?} option_contract_1m_rest=${K:-?} rest_fetch_audit=${A:-?}"; if [ "${T:-0}" = 0 ] && [ "${C:-0}" = 0 ] && [ "${S:-0}" = 0 ] && [ "${O:-0}" = 0 ] && [ "${K:-0}" = 0 ] && [ "${A:-0}" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain — inspect the counts + TRUNCATE-FAILED lines above'; fi"#,
];

/// legacy: `lambda_handler docker-reset cmds` (handler.py:1258-1306) — captured from the RUNNING oracle.
pub const DOCKER_RESET_COMMANDS: [&str; 17] = [
    r#"set +e"#,
    r#"systemctl stop tickvault || true"#,
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
pub const DOCKER_NUKE_BARE_COMMANDS: [&str; 10] = [
    r#"set +e"#,
    r#"systemctl stop tickvault || true"#,
    r#"systemctl disable tickvault || true"#,
    r#"docker ps -aq | xargs -r docker rm -f 2>/dev/null || true"#,
    r#"docker images -aq | xargs -r docker rmi -f 2>/dev/null || true"#,
    r#"docker volume ls -q | xargs -r docker volume rm -f 2>/dev/null || true"#,
    r#"docker system prune -af --volumes 2>/dev/null || true"#,
    r#"rm -rf /opt/tickvault/data/instrument-cache /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/ws_wal /opt/tickvault/data/groww 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"C=$(docker ps -aq 2>/dev/null | wc -l | tr -d ' '); I=$(docker images -aq 2>/dev/null | wc -l | tr -d ' '); V=$(docker volume ls -q 2>/dev/null | wc -l | tr -d ' '); echo "BARE-NUKE-RESULT containers=$C images=$I volumes=$V"; if [ "$C" = 0 ] && [ "$I" = 0 ] && [ "$V" = 0 ]; then echo bare-nuke-complete; else echo 'bare-nuke-PARTIAL: something is still present (likely in-use)'; fi"#,
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
