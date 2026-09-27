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
    r#"QDB='http://127.0.0.1:9000'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" | tail -n +2 | tr -d '"\r' | sed '/^$/d')
TARGETS=$(printf '%s\n' "$ALL" | awk '$0=="ticks" || $0=="market_depth" || (index($0,"candles_")==1 && $0!="candles_named") || $0=="prev_day_ohlcv" || $0=="rest_spot_1m" || $0=="rest_option_chain_1m" || $0=="rest_option_contract_1m" || $0=="rest_fetch_audit"' | sort)
echo "WIPE-TARGETS $(printf '%s\n' "$TARGETS" | sed '/^$/d' | wc -l | tr -d ' ') $(printf '%s\n' "$TARGETS" | sed '/^$/d' | paste -sd' ' -)"
for t in $TARGETS; do
  if curl -fsS --max-time 30 --get --data-urlencode "query=TRUNCATE TABLE $t" "$QDB/exec" >/dev/null; then echo "TRUNCATED $t"; else echo "TRUNCATE-FAILED $t"; fi
done"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl start tickvault || true"#,
    r#"sleep 3; qc() { curl -fsS "http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20$1" 2>/dev/null | grep -o '\[\[[0-9]*' | grep -o '[0-9]*'; }; T=$(qc ticks); D=$(qc market_depth); C=$(qc candles_1m); P=$(qc prev_day_ohlcv); S=$(qc rest_spot_1m); O=$(qc rest_option_chain_1m); K=$(qc rest_option_contract_1m); A=$(qc rest_fetch_audit); echo "WIPE-RESULT ticks=${T:-?} market_depth=${D:-?} candles_1m=${C:-?} prev_day_ohlcv=${P:-?} rest_spot_1m=${S:-?} rest_option_chain_1m=${O:-?} rest_option_contract_1m=${K:-?} rest_fetch_audit=${A:-?}"; if [ "${T:-0}" = 0 ] && [ "${D:-0}" = 0 ] && [ "${C:-0}" = 0 ] && [ "${P:-0}" = 0 ] && [ "${S:-0}" = 0 ] && [ "${O:-0}" = 0 ] && [ "${K:-0}" = 0 ] && [ "${A:-0}" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain — inspect the counts + TRUNCATE-FAILED lines above'; fi"#,
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
    // The abort rule WAS asymmetric: QuestDB unreachable => continue, on the
    // reasoning that there is nothing to export from a server that cannot
    // answer. That was wrong — the tables are still on the volume this
    // action then deletes. Since 2026-09-27 (audit PR20) an unreachable
    // QuestDB means the tables are copied straight off the volume, and ANY
    // step that cannot prove they are saved stops the action, restores the
    // box and pages (`sebi_abort`). The action is still the remedy for a
    // wedged QuestDB: it just saves the files before it deletes them.
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
SEBI='brutex_crossverify_cell_audit brutex_crossverify_daily cross_verify_1m_audit dhan_live_crossverify_cell_audit dhan_live_crossverify_daily dhan_rest_1m_tape feed_coverage_daily feed_episode_audit feed_parity_1m_audit feed_scoreboard_daily groww_cross_verify_1m_audit index_constituency instrument_fetch_audit instrument_lifecycle instrument_lifecycle_audit option_chain_1m option_contract_1m_rest order_audit order_leg_pnl order_update_events partition_archive_audit pnl_audit position_update_events prev_day_ohlcv rest_fetch_audit rest_option_chain_1m rest_option_contract_1m rest_spot_1m schema_reset_log spot_1m_rest spot_crossverify_cell_audit spot_crossverify_daily table_storage_daily tf_consistency_audit tick_conservation_audit ws_connection_daily ws_event_audit'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" 2>/dev/null | tail -n +2 | tr -d '"\r' | sed '/^$/d')
# 2026-09-27 (audit PR20): the "QuestDB did not answer => proceed" branch is
# GONE. It deleted the volume with every 5-year table still inside it and
# printed one line, which daily-universe Quote 25 names as a REJECT ("proceeds
# after a failed SEBI export"). A wedged QuestDB is exactly when the tables
# are still on disk, so this now copies them straight off the volume instead,
# with QuestDB stopped so the files are quiescent. Any step that cannot prove
# the tables are saved STOPS the action, restores the box and pages.
sebi_abort() {
  echo "ABORTED: $1"
  echo "code=LAMBDA-PORTAL-01 SEBI-PRESERVE-ABORT: nothing was deleted; the database and the app are being restarted."
  docker start tv-questdb >/dev/null 2>&1 || true
  systemctl enable tickvault >/dev/null 2>&1 || true
  systemctl start tickvault >/dev/null 2>&1 || true
  ACCT=$(timeout 20 aws sts get-caller-identity --query Account --output text 2>/dev/null)
  if [ -n "$ACCT" ]; then
    timeout 20 aws sns publish --region ap-south-1 --topic-arn "arn:aws:sns:ap-south-1:$ACCT:tv-prod-alerts" --subject 'Database reset stopped' --message "🔴 Database reset STOPPED before deleting anything. The 5-year records could not be saved first: $1 The app was restarted. Check the box before trying again." >/dev/null 2>&1 || echo 'SEBI-PRESERVE-PAGE-FAILED: the alert could not be sent'
  else
    echo 'SEBI-PRESERVE-PAGE-FAILED: the alert could not be sent'
  fi
  exit 1
}
if [ -z "$ALL" ]; then
  echo 'SEBI-PRESERVE-UNAVAILABLE: QuestDB did not answer — copying the 5-year tables straight off the database volume instead.'
  VOLS=$(docker volume ls -q 2>/dev/null) || sebi_abort 'Docker did not answer, so the database volume could not be checked.'
  if printf '%s\n' "$VOLS" | grep -qx tv-questdb-data; then
    docker stop tv-questdb >/dev/null 2>&1 || true
    MP=$(docker volume inspect -f '{{.Mountpoint}}' tv-questdb-data 2>/dev/null)
    [ -n "$MP" ] && [ -d "$MP/db" ] || sebi_abort 'The database files could not be found on the volume.'
    DIRS=''
    for t in $SEBI; do
      FOUND=0
      for d in "$MP/db/$t" "$MP/db/$t"~*; do
        [ -d "$d" ] || continue
        FOUND=1; DIRS="$DIRS $d"
      done
      [ "$FOUND" = 0 ] && echo "SEBI-ABSENT $t"
    done
    if [ -n "$DIRS" ]; then
      # shellcheck disable=SC2086
      NEED=$(du -sbc $DIRS 2>/dev/null | tail -n 1 | cut -f1)
      mkdir -p "$OUT/raw" || sebi_abort 'The save folder could not be created.'
      FREE=$(df -B1 --output=avail "$OUT/raw" 2>/dev/null | tail -n 1 | tr -d ' ')
      [ -n "$NEED" ] && [ -n "$FREE" ] && [ "$FREE" -gt "$NEED" ] || sebi_abort "Not enough free disk to save them (need ${NEED:-?} bytes, free ${FREE:-?})."
      for d in $DIRS; do
        n=$(basename "$d")
        if cp -a "$d" "$OUT/raw/$n" && [ "$(du -sb "$d" | cut -f1)" = "$(du -sb "$OUT/raw/$n" | cut -f1)" ]; then
          echo "SEBI-PRESERVED-RAW $n $(du -sb "$OUT/raw/$n" | cut -f1) bytes -> $OUT/raw/$n"
        else
          echo "SEBI-PRESERVE-FAILED $n"
          sebi_abort "Copying the table $n off the volume failed or came out a different size."
        fi
      done
    fi
  else
    echo 'SEBI-VOLUME-ABSENT: there is no database volume, so there is nothing to lose.'
  fi
else
  mkdir -p "$OUT" || sebi_abort 'The save folder could not be created.'
  for t in $SEBI; do
    if printf '%s\n' "$ALL" | grep -qx "$t"; then
      if curl -fsS --max-time 300 --get --data-urlencode "query=SELECT * FROM $t" "$QDB/exp" -o "$OUT/$t.csv"; then
        # Verified, not assumed: the file must hold at least as many rows as
        # the table (header excluded). A quoted newline inside a value can only
        # ADD lines, so a short file is a truncated export and never passes.
        ROWS=$(curl -fsS --max-time 30 --get --data-urlencode "query=SELECT count() FROM $t" "$QDB/exp" 2>/dev/null | tail -n 1 | tr -d '"\r ')
        LINES=$(awk 'END{print NR}' "$OUT/$t.csv")
        case "$ROWS" in ''|*[!0-9]*) echo "SEBI-PRESERVE-FAILED $t"; sebi_abort "The row count of $t could not be read to check its export." ;; esac
        if [ "$LINES" -ge 1 ] && [ $((LINES - 1)) -ge "$ROWS" ]; then
          echo "SEBI-PRESERVED $t $ROWS rows $(wc -c <"$OUT/$t.csv" | tr -d ' ') bytes -> $OUT/$t.csv"
        else
          echo "SEBI-PRESERVE-FAILED $t"
          sebi_abort "The export of $t came out short ($((LINES - 1)) of $ROWS rows)."
        fi
      else
        echo "SEBI-PRESERVE-FAILED $t"
        sebi_abort "The table $t exists and could not be exported."
      fi
    else
      echo "SEBI-ABSENT $t"
    fi
  done
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
    // The abort rule WAS asymmetric: QuestDB unreachable => continue, on the
    // reasoning that there is nothing to export from a server that cannot
    // answer. That was wrong — the tables are still on the volume this
    // action then deletes. Since 2026-09-27 (audit PR20) an unreachable
    // QuestDB means the tables are copied straight off the volume, and ANY
    // step that cannot prove they are saved stops the action, restores the
    // box and pages (`sebi_abort`). The action is still the remedy for a
    // wedged QuestDB: it just saves the files before it deletes them.
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
SEBI='brutex_crossverify_cell_audit brutex_crossverify_daily cross_verify_1m_audit dhan_live_crossverify_cell_audit dhan_live_crossverify_daily dhan_rest_1m_tape feed_coverage_daily feed_episode_audit feed_parity_1m_audit feed_scoreboard_daily groww_cross_verify_1m_audit index_constituency instrument_fetch_audit instrument_lifecycle instrument_lifecycle_audit option_chain_1m option_contract_1m_rest order_audit order_leg_pnl order_update_events partition_archive_audit pnl_audit position_update_events prev_day_ohlcv rest_fetch_audit rest_option_chain_1m rest_option_contract_1m rest_spot_1m schema_reset_log spot_1m_rest spot_crossverify_cell_audit spot_crossverify_daily table_storage_daily tf_consistency_audit tick_conservation_audit ws_connection_daily ws_event_audit'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" 2>/dev/null | tail -n +2 | tr -d '"\r' | sed '/^$/d')
# 2026-09-27 (audit PR20): the "QuestDB did not answer => proceed" branch is
# GONE. It deleted the volume with every 5-year table still inside it and
# printed one line, which daily-universe Quote 25 names as a REJECT ("proceeds
# after a failed SEBI export"). A wedged QuestDB is exactly when the tables
# are still on disk, so this now copies them straight off the volume instead,
# with QuestDB stopped so the files are quiescent. Any step that cannot prove
# the tables are saved STOPS the action, restores the box and pages.
sebi_abort() {
  echo "ABORTED: $1"
  echo "code=LAMBDA-PORTAL-01 SEBI-PRESERVE-ABORT: nothing was deleted; the database and the app are being restarted."
  docker start tv-questdb >/dev/null 2>&1 || true
  systemctl enable tickvault >/dev/null 2>&1 || true
  systemctl start tickvault >/dev/null 2>&1 || true
  ACCT=$(timeout 20 aws sts get-caller-identity --query Account --output text 2>/dev/null)
  if [ -n "$ACCT" ]; then
    timeout 20 aws sns publish --region ap-south-1 --topic-arn "arn:aws:sns:ap-south-1:$ACCT:tv-prod-alerts" --subject 'Database reset stopped' --message "🔴 Database reset STOPPED before deleting anything. The 5-year records could not be saved first: $1 The app was restarted. Check the box before trying again." >/dev/null 2>&1 || echo 'SEBI-PRESERVE-PAGE-FAILED: the alert could not be sent'
  else
    echo 'SEBI-PRESERVE-PAGE-FAILED: the alert could not be sent'
  fi
  exit 1
}
if [ -z "$ALL" ]; then
  echo 'SEBI-PRESERVE-UNAVAILABLE: QuestDB did not answer — copying the 5-year tables straight off the database volume instead.'
  VOLS=$(docker volume ls -q 2>/dev/null) || sebi_abort 'Docker did not answer, so the database volume could not be checked.'
  if printf '%s\n' "$VOLS" | grep -qx tv-questdb-data; then
    docker stop tv-questdb >/dev/null 2>&1 || true
    MP=$(docker volume inspect -f '{{.Mountpoint}}' tv-questdb-data 2>/dev/null)
    [ -n "$MP" ] && [ -d "$MP/db" ] || sebi_abort 'The database files could not be found on the volume.'
    DIRS=''
    for t in $SEBI; do
      FOUND=0
      for d in "$MP/db/$t" "$MP/db/$t"~*; do
        [ -d "$d" ] || continue
        FOUND=1; DIRS="$DIRS $d"
      done
      [ "$FOUND" = 0 ] && echo "SEBI-ABSENT $t"
    done
    if [ -n "$DIRS" ]; then
      # shellcheck disable=SC2086
      NEED=$(du -sbc $DIRS 2>/dev/null | tail -n 1 | cut -f1)
      mkdir -p "$OUT/raw" || sebi_abort 'The save folder could not be created.'
      FREE=$(df -B1 --output=avail "$OUT/raw" 2>/dev/null | tail -n 1 | tr -d ' ')
      [ -n "$NEED" ] && [ -n "$FREE" ] && [ "$FREE" -gt "$NEED" ] || sebi_abort "Not enough free disk to save them (need ${NEED:-?} bytes, free ${FREE:-?})."
      for d in $DIRS; do
        n=$(basename "$d")
        if cp -a "$d" "$OUT/raw/$n" && [ "$(du -sb "$d" | cut -f1)" = "$(du -sb "$OUT/raw/$n" | cut -f1)" ]; then
          echo "SEBI-PRESERVED-RAW $n $(du -sb "$OUT/raw/$n" | cut -f1) bytes -> $OUT/raw/$n"
        else
          echo "SEBI-PRESERVE-FAILED $n"
          sebi_abort "Copying the table $n off the volume failed or came out a different size."
        fi
      done
    fi
  else
    echo 'SEBI-VOLUME-ABSENT: there is no database volume, so there is nothing to lose.'
  fi
else
  mkdir -p "$OUT" || sebi_abort 'The save folder could not be created.'
  for t in $SEBI; do
    if printf '%s\n' "$ALL" | grep -qx "$t"; then
      if curl -fsS --max-time 300 --get --data-urlencode "query=SELECT * FROM $t" "$QDB/exp" -o "$OUT/$t.csv"; then
        # Verified, not assumed: the file must hold at least as many rows as
        # the table (header excluded). A quoted newline inside a value can only
        # ADD lines, so a short file is a truncated export and never passes.
        ROWS=$(curl -fsS --max-time 30 --get --data-urlencode "query=SELECT count() FROM $t" "$QDB/exp" 2>/dev/null | tail -n 1 | tr -d '"\r ')
        LINES=$(awk 'END{print NR}' "$OUT/$t.csv")
        case "$ROWS" in ''|*[!0-9]*) echo "SEBI-PRESERVE-FAILED $t"; sebi_abort "The row count of $t could not be read to check its export." ;; esac
        if [ "$LINES" -ge 1 ] && [ $((LINES - 1)) -ge "$ROWS" ]; then
          echo "SEBI-PRESERVED $t $ROWS rows $(wc -c <"$OUT/$t.csv" | tr -d ' ') bytes -> $OUT/$t.csv"
        else
          echo "SEBI-PRESERVE-FAILED $t"
          sebi_abort "The export of $t came out short ($((LINES - 1)) of $ROWS rows)."
        fi
      else
        echo "SEBI-PRESERVE-FAILED $t"
        sebi_abort "The table $t exists and could not be exported."
      fi
    else
      echo "SEBI-ABSENT $t"
    fi
  done
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
