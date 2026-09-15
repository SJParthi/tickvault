//! Operator action command lists, originally captured from the legacy oracle.
//! Subsequent reviewed safety fixes are maintained here and exercised by the
//! operator-control regressions; these are the command lists actually dispatched.

/// Read-only preparation for the wipe. Strict catalog parsing happens BEFORE
/// stopping the app or removing replay data. The original target manifest is
/// retained for verification even if a table disappears from the later catalog.
///
/// Framing the raw curl stream avoids command substitution dropping NUL bytes
/// or trailing blank rows. A unique final success record must follow a complete
/// CSV response; an HTTP/transport failure cannot be hidden by a valid-looking
/// body. Both CSV parsers reject extra fields, records, and malformed framing.
pub(crate) const WIPE_QUESTDB_PREPARE_COMMAND: &str = r#"QDB='http://127.0.0.1:9000'
WIPE_REQUIRED_TARGETS='ticks market_depth candles_1m prev_day_ohlcv rest_spot_1m rest_option_chain_1m rest_option_contract_1m rest_fetch_audit'
qdb_wipe_csv() {
  if curl -fsS --max-time "$2" --get --data-urlencode "query=$1" "$QDB/exp" 2>/dev/null; then
    printf '\n__TICKVAULT_WIPE_CURL_OK__\n'
  else
    printf '\n__TICKVAULT_WIPE_CURL_ERROR__\n'
  fi
}
wipe_targets() {
  qdb_wipe_csv 'SELECT table_name FROM tables()' 15 | LC_ALL=C awk '
    { sub(/\r$/, "") }
    NR == 1 { if ($0 != "\"table_name\"") invalid = 1; next }
    $0 == "__TICKVAULT_WIPE_CURL_OK__" {
      if (footer) invalid = 1
      footer = NR
      next
    }
    $0 == "" { if (blank) invalid = 1; blank = NR; next }
    {
      if (footer || blank) invalid = 1
      if ($0 !~ /^"[A-Za-z_][A-Za-z0-9_]*"$/) { invalid = 1; next }
      $0 = substr($0, 2, length($0) - 2)
      if (seen[$0]++) invalid = 1
      if ($0=="ticks" || $0=="market_depth" || index($0,"candles_")==1 || $0=="prev_day_ohlcv" || $0=="rest_spot_1m" || $0=="rest_option_chain_1m" || $0=="rest_option_contract_1m" || $0=="rest_fetch_audit") targets[++n] = $0
    }
    END {
      if (invalid || !footer || footer != NR || (blank && blank != NR - 1) || !n) exit 1
      for (i = 1; i <= n; i++) print targets[i]
    }
  '
}
qc() {
  case "$1" in
    ticks|market_depth|prev_day_ohlcv|rest_spot_1m|rest_option_chain_1m|rest_option_contract_1m|rest_fetch_audit|candles_*) ;;
    *) return 1 ;;
  esac
  case "$1" in *[!A-Za-z0-9_]*) return 1 ;; esac
  qdb_wipe_csv "SELECT count() AS count FROM $1" 5 | LC_ALL=C awk '
    { sub(/\r$/, "") }
    NR == 1 { if ($0 != "\"count\"") invalid = 1; next }
    NR == 2 {
      if ($0 !~ /^(0|[1-9][0-9]*)$/) invalid = 1
      count = $0
      next
    }
    NR == 3 && $0 == "" { blank = 1; next }
    $0 == "__TICKVAULT_WIPE_CURL_OK__" && (NR == 3 || (NR == 4 && blank)) { footer = 1; next }
    { invalid = 1 }
    END { if (!invalid && footer && (NR == 3 || NR == 4)) print count; else exit 1 }
  '
}
TARGETS=$(wipe_targets) || { echo 'WIPE-PARTIAL: target catalog could not be verified; no wipe started'; exit 1; }
TARGETS=$(printf '%s\n' "$TARGETS" | LC_ALL=C sort -u) || { echo 'WIPE-PARTIAL: target manifest could not be prepared; no wipe started'; exit 1; }
for t in $WIPE_REQUIRED_TARGETS; do
  if ! printf '%s\n' "$TARGETS" | grep -Fxq -- "$t"; then
    echo "WIPE-PARTIAL: required table $t is absent; no wipe started"
    exit 1
  fi
done
WIPE_MANIFEST_READY=1
WIPE_TRUNCATES_OK=0"#;

/// Legacy wipe action with reviewed catalog preflight and complete verification.
pub const WIPE_QUESTDB_COMMANDS: [&str; 11] = [
    r#"set +e"#,
    WIPE_QUESTDB_PREPARE_COMMAND,
    r#"systemctl stop tickvault || true"#,
    r#"systemctl disable tickvault || true"#,
    r#"rm -rf /opt/tickvault/data/ws_wal /opt/tickvault/data/groww /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/instrument-cache 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"echo 'OK: feed capture/replay sources removed (ws_wal, groww, spill, dlq, instrument-cache)'"#,
    // 2026-08-01 (operator directive — pure Rust, nowhere the banned runtime):
    // this element WAS a 17-line embedded interpreter program dispatched via
    // SSM RunCommand to the prod box — i.e. the banned runtime EXECUTING in
    // production. Re-expressed as curl + POSIX shell with the SAME semantics:
    // same dynamic target policy, same TRUNCATE per
    // target, same WIPE-TARGETS / TRUNCATED / TRUNCATE-FAILED stdout markers.
    // The preparation command validates the CSV catalog and every identifier
    // before any mutation. The verification tail counts every captured target
    // plus targets discovered after restart, including every candles_* table.
    //
    // 2026-09-05: `market_depth` was MISSING from both halves -- the target
    // predicate and the verification tail -- so `wipe-questdb` truncated
    // everything else and printed WIPE-COMPLETE with the LARGEST table in the
    // process untouched (measured 1,530,651,649 rows/session, 299 GB). That is
    // a false OK on a destructive tool, and it is why the 2026-09-05 wipe had
    // to drop the table by hand over EC2 Instance Connect after this action
    // reported success. Quote 21 of that day names `market_depth` in the
    // authorized wipe set explicitly, so the omission was a defect against the
    // operator's own written scope, not a deliberate carve-out.
    //
    // It is in BOTH halves now. Verification-only would have been worse than
    // useless: the tool would report WIPE-PARTIAL forever while never
    // truncating the table it complains about.
    r#"echo "WIPE-TARGETS $(printf '%s\n' "$TARGETS" | wc -l | tr -d ' ') $(printf '%s\n' "$TARGETS" | paste -sd' ' -)"
WIPE_TRUNCATES_OK=1
for t in $TARGETS; do
  if curl -fsS --max-time 30 --get --data-urlencode "query=TRUNCATE TABLE $t" "$QDB/exec" >/dev/null; then echo "TRUNCATED $t"; else echo "TRUNCATE-FAILED $t"; WIPE_TRUNCATES_OK=0; fi
done"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl start tickvault || true"#,
    // Unknown is never zero. Verify the union of original and current targets:
    // disappearing tables still get counted (and fail), while new candle
    // tables cannot escape through a representative candles_1m-only check.
    // App restart and concurrent writers mean these are observed counts, not
    // an atomic snapshot or a promise that rows cannot appear afterwards.
    r#"sleep 3
if [ "${WIPE_MANIFEST_READY:-0}" != 1 ] || [ -z "${TARGETS:-}" ]; then
  echo 'WIPE-PARTIAL: original target manifest is unavailable'
  exit 1
fi
CURRENT_TARGETS=$(wipe_targets) || { echo 'WIPE-PARTIAL: verification catalog could not be verified'; exit 1; }
VERIFY_TARGETS=$(printf '%s\n' "$TARGETS" "$CURRENT_TARGETS" | LC_ALL=C sort -u) || { echo 'WIPE-PARTIAL: verification manifest could not be prepared'; exit 1; }
WIPE_PARTIAL=0
if [ "${WIPE_TRUNCATES_OK:-0}" != 1 ]; then WIPE_PARTIAL=1; fi
printf 'WIPE-RESULT'
for t in $VERIFY_TARGETS; do
  if COUNT=$(qc "$t"); then
    if [ "$COUNT" != 0 ]; then WIPE_PARTIAL=1; fi
  else
    COUNT='?'
    WIPE_PARTIAL=1
  fi
  printf ' %s=%s' "$t" "$COUNT"
done
printf '\n'
if [ "$WIPE_PARTIAL" = 0 ]; then
  echo WIPE-COMPLETE
else
  echo 'WIPE-PARTIAL: rows remain or a count could not be verified — inspect the counts + TRUNCATE-FAILED lines above'
  exit 1
fi"#,
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
# 2026-09-05: was FOUR names. The other 32 tables the repository's own
# retention lists declare must never be lost were destroyed with the
# volume, unexported, while this action printed SEBI-PRESERVED four times
# and exited 0. The abort rule only ever inspected the four it named, so
# an operator saw a clean run. This set is now the UNION of
# DAY_PARTITIONED_TABLES and RETENTION_EXEMPT_TABLES in
# crates/storage/src/partition_manager.rs — the repo's authoritative
# never-delete definition — and a lockstep guard derives the expected
# set from that source rather than restating it, so the two cannot drift
# and the guard can never again assert a list against itself.
SEBI='brutex_crossverify_cell_audit brutex_crossverify_daily cross_verify_1m_audit dhan_live_crossverify_cell_audit dhan_live_crossverify_daily dhan_rest_1m_tape feed_coverage_daily feed_episode_audit feed_parity_1m_audit feed_scoreboard_daily groww_cross_verify_1m_audit index_constituency instrument_fetch_audit instrument_lifecycle instrument_lifecycle_audit option_chain_1m option_contract_1m_rest order_audit order_leg_pnl order_update_events partition_archive_audit pnl_audit position_update_events prev_day_ohlcv rest_fetch_audit rest_option_chain_1m rest_option_contract_1m rest_spot_1m spot_1m_rest spot_crossverify_cell_audit spot_crossverify_daily table_storage_daily tf_consistency_audit tick_conservation_audit ws_connection_daily ws_event_audit'
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
# 2026-09-05: was FOUR names. The other 32 tables the repository's own
# retention lists declare must never be lost were destroyed with the
# volume, unexported, while this action printed SEBI-PRESERVED four times
# and exited 0. The abort rule only ever inspected the four it named, so
# an operator saw a clean run. This set is now the UNION of
# DAY_PARTITIONED_TABLES and RETENTION_EXEMPT_TABLES in
# crates/storage/src/partition_manager.rs — the repo's authoritative
# never-delete definition — and a lockstep guard derives the expected
# set from that source rather than restating it, so the two cannot drift
# and the guard can never again assert a list against itself.
SEBI='brutex_crossverify_cell_audit brutex_crossverify_daily cross_verify_1m_audit dhan_live_crossverify_cell_audit dhan_live_crossverify_daily dhan_rest_1m_tape feed_coverage_daily feed_episode_audit feed_parity_1m_audit feed_scoreboard_daily groww_cross_verify_1m_audit index_constituency instrument_fetch_audit instrument_lifecycle instrument_lifecycle_audit option_chain_1m option_contract_1m_rest order_audit order_leg_pnl order_update_events partition_archive_audit pnl_audit position_update_events prev_day_ohlcv rest_fetch_audit rest_option_chain_1m rest_option_contract_1m rest_spot_1m spot_1m_rest spot_crossverify_cell_audit spot_crossverify_daily table_storage_daily tf_consistency_audit tick_conservation_audit ws_connection_daily ws_event_audit'
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
