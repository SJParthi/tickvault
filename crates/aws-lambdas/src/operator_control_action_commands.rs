//! AUTO-GENERATED action command goldens for the operator-control port —
//! captured by RUNNING the python oracle's `lambda_handler`
//! (`deploy/aws/lambda/operator-control/handler.py`) with a stubbed
//! `_ssm_shell` (`scratchpad/w4-dump-actions.py`), NEVER hand-transcribed.
//! Byte-exact with the SSM command lists each action dispatches.

/// python: `lambda_handler wipe-questdb cmds` (handler.py:1126-1197) — captured from the RUNNING oracle.
pub const WIPE_QUESTDB_COMMANDS: [&str; 10] = [
    r#"set +e"#,
    r#"systemctl stop tickvault || true"#,
    r#"systemctl disable tickvault || true"#,
    r#"rm -rf /opt/tickvault/data/ws_wal /opt/tickvault/data/groww /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/instrument-cache 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"echo 'OK: feed capture/replay sources removed (ws_wal, groww, spill, dlq, instrument-cache)'"#,
    r#"python3 - <<'PYWIPE'
import json, urllib.request, urllib.parse
base = 'http://127.0.0.1:9000/exec?query='
q = urllib.parse.quote("SELECT table_name FROM tables()")
rows = json.load(urllib.request.urlopen(base + q, timeout=15)).get('dataset', [])
names = [r[0] for r in rows if isinstance(r, list) and r]
live_rest = {'spot_1m_rest', 'option_chain_1m', 'option_contract_1m_rest', 'rest_fetch_audit'}
targets = [t for t in names if t == 'ticks' or t.startswith('candles_') or t == 'prev_day_ohlcv' or t in live_rest]
print('WIPE-TARGETS', len(targets), sorted(targets))
for t in targets:
    tq = urllib.parse.quote(f'TRUNCATE TABLE {t}')
    try:
        urllib.request.urlopen(base + tq, timeout=30).read()
        print('TRUNCATED', t)
    except Exception as exc:
        print('TRUNCATE-FAILED', t, exc)
PYWIPE"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl start tickvault || true"#,
    r#"sleep 3; qc() { curl -fsS "http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20$1" 2>/dev/null | grep -o '\[\[[0-9]*' | grep -o '[0-9]*'; }; T=$(qc ticks); C=$(qc candles_1m); S=$(qc spot_1m_rest); O=$(qc option_chain_1m); K=$(qc option_contract_1m_rest); A=$(qc rest_fetch_audit); echo "WIPE-RESULT ticks=${T:-?} candles_1m=${C:-?} spot_1m_rest=${S:-?} option_chain_1m=${O:-?} option_contract_1m_rest=${K:-?} rest_fetch_audit=${A:-?}"; if [ "${T:-0}" = 0 ] && [ "${C:-0}" = 0 ] && [ "${S:-0}" = 0 ] && [ "${O:-0}" = 0 ] && [ "${K:-0}" = 0 ] && [ "${A:-0}" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain — inspect the counts + TRUNCATE-FAILED lines above'; fi"#,
];

/// python: `lambda_handler docker-reset cmds` (handler.py:1258-1306) — captured from the RUNNING oracle.
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

/// python: `lambda_handler docker-nuke-bare cmds` (handler.py:1338-1368) — captured from the RUNNING oracle.
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

/// python: `lambda_handler logs cmds` (handler.py:1414-1424) — captured from the RUNNING oracle.
pub const LOGS_COMMANDS: [&str; 7] = [
    r#"set +e"#,
    r#"echo ERR_BEGIN"#,
    r#"journalctl -u tickvault -p err -n 40 --no-pager 2>/dev/null | tail -40 || true"#,
    r#"echo ERR_END"#,
    r#"echo APP_BEGIN"#,
    r#"journalctl -u tickvault -n 40 --no-pager 2>/dev/null | tail -40 || true"#,
    r#"echo APP_END"#,
];
