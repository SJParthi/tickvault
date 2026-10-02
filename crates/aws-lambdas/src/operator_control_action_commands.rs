//! AUTO-GENERATED action command goldens for the operator-control port —
//! captured by RUNNING the legacy oracle's `lambda_handler`
//! (`deploy/aws/lambda/operator-control/handler.py`) with a stubbed
//! `_ssm_shell` (`scratchpad/w4-dump-actions.py`), NEVER hand-transcribed.
//! Byte-exact with the SSM command lists each action dispatches.

/// Checked on the box, before the first `systemctl stop`, by every action
/// that deletes data (wipe, reset, nuke).
///
/// The console refuses these actions 09:00–15:45 IST, but only at the moment
/// it sends them. SSM can deliver a command minutes later (the box was
/// booting, the agent was reconnecting), so the refusal at send time does
/// not stop a late delivery from running inside the lock. This line re-reads
/// the box clock and stops before anything has been stopped or deleted, so a
/// refused run leaves the app exactly as it was. The window is the same
/// seconds-of-day as `DATA_DESTRUCTIVE_LOCK_OPEN_SECS` /
/// `DATA_DESTRUCTIVE_LOCK_CLOSE_SECS` (pinned by
/// `test_on_box_lock_guard_runs_before_the_first_stop`).
///
/// Exit 3, not 1: the reset's SEBI step reserves `exit 1` for `sebi_abort`.
pub const ON_BOX_LOCK_GUARD: &str = r#"NOW=$(date -u +%s); case "$NOW" in ''|*[!0-9]*) echo 'LOCKED-ON-BOX: the box clock could not be read, so nothing was stopped or deleted.'; exit 3 ;; esac; SOD=$(((NOW + 19800) % 86400)); if [ "$SOD" -ge 32400 ] && [ "$SOD" -lt 56700 ]; then echo 'LOCKED-ON-BOX: this reached the box inside the 09:00-15:45 IST lock, so nothing was stopped or deleted. Run it again after 15:45.'; exit 3; fi"#;

/// legacy: `lambda_handler wipe-questdb cmds` (handler.py:1126-1197) — captured from the RUNNING oracle.
pub const WIPE_QUESTDB_COMMANDS: [&str; 11] = [
    r#"set +e"#,
    ON_BOX_LOCK_GUARD,
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
    //
    // 2026-09-27 (audit PR28): the wipe no longer truncates `prev_day_ohlcv`
    // or the four `rest_*` tables. Every one of them is on the never-delete
    // list (`DAY_PARTITIONED_TABLES` in the storage crate's
    // partition_manager.rs, the same list the reset saves before it deletes
    // anything), and daily-universe Quote 21 names `rest_fetch_audit` and the
    // REST minute tables as KEPT by a fresh-start wipe. The 2026-07-16
    // extension that added them predates both. The targets are now the
    // market data only: `ticks`, `market_depth` and the candle tables.
    // `test_wipe_never_targets_a_never_delete_table` reads the storage list
    // and fails if any target is on it.
    r#"QDB='http://127.0.0.1:9000'
ALL=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT table_name FROM tables()' "$QDB/exp" | tail -n +2 | tr -d '"\r' | sed '/^$/d')
TARGETS=$(printf '%s\n' "$ALL" | awk '$0=="ticks" || $0=="market_depth" || (index($0,"candles_")==1 && $0!="candles_named")' | sort)
echo "WIPE-TARGETS $(printf '%s\n' "$TARGETS" | sed '/^$/d' | wc -l | tr -d ' ') $(printf '%s\n' "$TARGETS" | sed '/^$/d' | paste -sd' ' -)"
for t in $TARGETS; do
  if curl -fsS --max-time 30 --get --data-urlencode "query=TRUNCATE TABLE $t" "$QDB/exec" >/dev/null; then echo "TRUNCATED $t"; else echo "TRUNCATE-FAILED $t"; fi
done"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl start tickvault || true"#,
    r#"sleep 3; qc() { curl -fsS "http://127.0.0.1:9000/exec?query=SELECT%20count()%20FROM%20$1" 2>/dev/null | grep -o '\[\[[0-9]*' | grep -o '[0-9]*'; }; T=$(qc ticks); D=$(qc market_depth); C=$(qc candles_1m); echo "WIPE-RESULT ticks=${T:-?} market_depth=${D:-?} candles_1m=${C:-?}"; if [ "${T:-0}" = 0 ] && [ "${D:-0}" = 0 ] && [ "${C:-0}" = 0 ]; then echo WIPE-COMPLETE; else echo 'WIPE-PARTIAL: rows remain — inspect the counts + TRUNCATE-FAILED lines above'; fi"#,
];

/// legacy: `lambda_handler docker-reset cmds` (handler.py:1258-1306) — captured from the RUNNING oracle.
pub const DOCKER_RESET_COMMANDS: [&str; 18] = [
    r#"set +e"#,
    ON_BOX_LOCK_GUARD,
    // 2026-09-27 (audit PR20): also DISABLED while the action runs. The
    // 15-minute autopilot restarts a stopped-but-enabled app, and the app
    // brings QuestDB back up, which would write the volume mid-save. A
    // disabled unit reads as intentional and is left alone; every path out
    // of this action (success below, or `sebi_abort`) re-enables it.
    r#"systemctl stop tickvault || true; systemctl disable tickvault || true"#,
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
  echo "code=LAMBDA-PORTAL-01 SEBI-PRESERVE-ABORT: nothing was deleted; the database and the app are being restarted. Any partial save is left in $OUT."
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
# One reset at a time. A second run that finds the lock held stops before it
# touches anything and leaves the box to the run that holds it (exit 2, not
# sebi_abort: restarting the app here would fight the other run).
if command -v flock >/dev/null 2>&1; then
  exec 9>/run/tv-sebi-preserve.lock
  flock -n 9 || { echo 'SEBI-PRESERVE-BUSY: another database reset is already running; this one stopped before touching anything.'; exit 2; }
fi
# The whole save must finish inside the SSM command's default 3,600 s
# execution timeout, or SSM kills the script mid-copy and sebi_abort never
# runs. 45 minutes leaves the restore and the page room to run.
START=$(date +%s); BUDGET=2700
left() { echo $((START + BUDGET - $(date +%s))); }
budget_check() { [ "$(left)" -gt 60 ] || sebi_abort 'The save ran out of time (45 minutes) before it finished.'; }
# The console refuses this action 09:00-15:45 IST, but only when it is sent.
# A long save can carry the deletion past 09:00, so the box re-checks the
# same window (DATA_DESTRUCTIVE_LOCK_OPEN_SECS / _CLOSE_SECS, seconds of the
# IST day) right before the first step that deletes anything.
lock_check() {
  NOW=$(date -u +%s)
  case "$NOW" in ''|*[!0-9]*) sebi_abort 'The box clock could not be read to re-check the 09:00-15:45 IST lock.' ;; esac
  SOD=$(((NOW + 19800) % 86400))
  if [ "$SOD" -ge 32400 ] && [ "$SOD" -lt 56700 ]; then
    sebi_abort 'The save finished inside the 09:00-15:45 IST lock, so nothing was deleted. Run it again after 15:45.'
  fi
}
# Files and bytes under a directory, file contents only. `du -sb` also counts
# directory entries, whose size differs between the source and a fresh copy,
# so it cannot prove a copy complete.
fbytes() { find "$1" -type f -printf '%s\n' 2>/dev/null | awk '{s += $1; n++} END {print n + 0 ":" s + 0}'; }
# Everything that could be writing the volume: any running container that
# mounts it, plus the database container's own run state and start time. If
# this changes during a copy, something restarted the database (the 15-minute
# autopilot starts the database container when it finds it down) and the copy
# is of files that were moving.
qdb_quiet() { printf '%s|%s' "$(docker ps -q --filter volume=tv-questdb-data 2>/dev/null | tr -d '\n')" "$(docker inspect -f '{{.State.Running}} {{.State.StartedAt}}' tv-questdb 2>/dev/null)"; }
# The table folders of the 5-year tables on the volume (a WAL table's folder
# holds its unapplied WAL segments too), into DIRS.
sebi_dirs() {
  DIRS=''
  for t in $SEBI; do
    FOUND=0
    for d in "$MP/db/$t" "$MP/db/$t"~*; do
      [ -d "$d" ] || continue
      FOUND=1; DIRS="$DIRS $d"
    done
    [ "$FOUND" = 0 ] && echo "SEBI-ABSENT $t"
  done
}
# Free space must cover the save plus a fifth, and still leave 5 GiB for the
# app afterwards.
room_for() {
  FREE=$(df -B1 --output=avail "$1" 2>/dev/null | tail -n 1 | tr -d ' ')
  WANT=$(($2 + $2 / 5 + 5368709120))
  case "$FREE" in ''|*[!0-9]*) sebi_abort 'The free disk space could not be read.' ;; esac
  [ "$FREE" -gt "$WANT" ] || sebi_abort "Not enough free disk to save them (need $WANT bytes with margin, free $FREE)."
}
# How many 5-year WAL tables still hold rows QuestDB has not applied, or are
# suspended. `SELECT *` and `count()` both read only APPLIED rows, so an
# export of a table that is behind misses rows and its own check passes.
wal_behind() {
  WT=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT name, suspended, writerTxn, sequencerTxn FROM wal_tables()' "$QDB/exp" 2>/dev/null) || { echo unknown; return; }
  printf '%s\n' "$WT" | tail -n +2 | tr -d '"\r' | awk -F, -v s=" $SEBI " 'index(s, " " $1 " ") && ($2 == "true" || $3 != $4) {n++} END {print n + 0}'
}
# Stop the database so its files are quiescent, and record what is running
# against the volume so a restart during the copy is caught.
stop_qdb() {
  docker stop tv-questdb >/dev/null 2>&1 || true
  [ -z "$(docker ps -q --filter volume=tv-questdb-data 2>/dev/null)" ] || sebi_abort 'The database could not be stopped, so its files cannot be copied safely.'
  Q0=$(qdb_quiet)
}
# Copy the table folders in DIRS off the volume into $OUT/raw, each checked
# file for file and byte for byte against its source. The database must be
# stopped first (stop_qdb).
raw_copy() {
  # shellcheck disable=SC2086
  NEED=$(du -sbc $DIRS 2>/dev/null | tail -n 1 | cut -f1)
  case "$NEED" in ''|*[!0-9]*) sebi_abort 'The size of the tables to save could not be read.' ;; esac
  mkdir -p "$OUT/raw" || sebi_abort 'The save folder could not be created.'
  room_for "$OUT/raw" "$NEED"
  # The name-to-folder registry, so a restore can map the folders back.
  for f in "$MP/db"/tables.d* "$MP/db"/_tab_index.d; do
    [ -f "$f" ] || continue
    cp -a "$f" "$OUT/raw/" || sebi_abort 'The table registry could not be copied off the volume.'
  done
  for d in $DIRS; do
    budget_check
    [ "$(qdb_quiet)" = "$Q0" ] || sebi_abort 'Something restarted the database during the copy, so the copied files may be incomplete.'
    n=$(basename "$d")
    if timeout "$(left)" cp -a "$d" "$OUT/raw/$n" && [ "$(fbytes "$d")" = "$(fbytes "$OUT/raw/$n")" ]; then
      echo "SEBI-PRESERVED-RAW $n $(fbytes "$OUT/raw/$n") files:bytes -> $OUT/raw/$n"
    else
      echo "SEBI-PRESERVE-FAILED $n"
      sebi_abort "Copying the table $n off the volume failed or came out different."
    fi
  done
  [ "$(qdb_quiet)" = "$Q0" ] || sebi_abort 'Something restarted the database during the copy, so the copied files may be incomplete.'
}
MODE=export; EXPORTED=''
if [ -z "$ALL" ]; then
  MODE=raw
  echo 'SEBI-PRESERVE-UNAVAILABLE: QuestDB did not answer — copying the 5-year tables straight off the database volume instead.'
else
  i=0; BEHIND=$(wal_behind)
  while [ "$BEHIND" != 0 ] && [ "$i" -lt 24 ]; do sleep 5; i=$((i + 1)); BEHIND=$(wal_behind); done
  if [ "$BEHIND" != 0 ]; then
    MODE=raw
    echo "SEBI-PRESERVE-WAL-BEHIND: $BEHIND of the 5-year tables still have rows QuestDB has not applied (or it could not say), so an export would miss them — copying them straight off the database volume instead."
  fi
fi
if [ "$MODE" = raw ]; then
  VOLS=$(docker volume ls -q 2>/dev/null) || sebi_abort 'Docker did not answer, so the database volume could not be checked.'
  if printf '%s\n' "$VOLS" | grep -qx tv-questdb-data; then
    stop_qdb
    MP=$(docker volume inspect -f '{{.Mountpoint}}' tv-questdb-data 2>/dev/null)
    [ -n "$MP" ] && [ -d "$MP/db" ] || sebi_abort 'The database files could not be found on the volume.'
    sebi_dirs
    if [ -n "$DIRS" ]; then
      raw_copy
    fi
  else
    echo 'SEBI-VOLUME-ABSENT: there is no database volume, so there is nothing to lose.'
  fi
else
  mkdir -p "$OUT" || sebi_abort 'The save folder could not be created.'
  # Size the export from the tables' own folders when the volume can be read;
  # otherwise only the 5 GiB floor applies.
  EST=0
  MP=$(docker volume inspect -f '{{.Mountpoint}}' tv-questdb-data 2>/dev/null)
  if [ -n "$MP" ] && [ -d "$MP/db" ]; then
    sebi_dirs >/dev/null
    # shellcheck disable=SC2086
    [ -n "$DIRS" ] && EST=$(du -sbc $DIRS 2>/dev/null | tail -n 1 | cut -f1)
    case "$EST" in ''|*[!0-9]*) EST=0 ;; esac
  fi
  room_for "$OUT" "$EST"
  for t in $SEBI; do
    if printf '%s\n' "$ALL" | grep -qx "$t"; then
      budget_check
      if curl -fsS --max-time 300 --get --data-urlencode "query=SELECT * FROM $t" "$QDB/exp" -o "$OUT/$t.csv"; then
        # Verified, not assumed: the file must hold at least as many rows as
        # the table (header excluded). A quoted newline inside a value can only
        # ADD lines, so a short file is a truncated export and never passes.
        # The WAL check above is what makes count() the whole table.
        ROWS=$(curl -fsS --max-time 30 --get --data-urlencode "query=SELECT count() FROM $t" "$QDB/exp" 2>/dev/null | tail -n 1 | tr -d '"\r ')
        LINES=$(awk 'END{print NR}' "$OUT/$t.csv")
        case "$ROWS" in ''|*[!0-9]*) echo "SEBI-PRESERVE-FAILED $t"; sebi_abort "The row count of $t could not be read to check its export." ;; esac
        if [ "$LINES" -ge 1 ] && [ $((LINES - 1)) -ge "$ROWS" ]; then
          echo "SEBI-PRESERVED $t $ROWS rows $(wc -c <"$OUT/$t.csv" | tr -d ' ') bytes -> $OUT/$t.csv"
          EXPORTED="$EXPORTED $t"
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
  # 2026-09-27 (audit PR28): the table list above came from ONE bounded
  # query. A list cut short (a timeout mid-body, a partial reply), a table
  # whose metadata QuestDB could not load, or a folder left under a table's
  # old name after a rename all made a kept table read as absent, and the
  # volume holding it was then deleted. Every folder of a kept table that was
  # not exported is now copied off the volume as it is, the same way the
  # raw mode copies it. When the volume exists but its folders cannot be
  # read, nothing can be proven, so that stops the action.
  if [ -n "$MP" ] && [ -d "$MP/db" ]; then
    DIRS=''
    for t in $SEBI; do
      case " $EXPORTED " in *" $t "*) continue ;; esac
      for d in "$MP/db/$t" "$MP/db/$t"~*; do
        [ -d "$d" ] && DIRS="$DIRS $d"
      done
    done
    if [ -n "$DIRS" ]; then
      echo "SEBI-PRESERVE-UNLISTED:$DIRS are folders of 5-year tables that QuestDB did not list, so they were not exported. Copying them straight off the database volume."
      stop_qdb
      raw_copy
    fi
  elif docker volume inspect tv-questdb-data >/dev/null 2>&1; then
    sebi_abort 'The database volume exists but its folders could not be read, so it cannot be proven that every 5-year table was saved.'
  fi
fi
# 2026-09-27 (audit PR28): a copy on this box's own disk is lost with the
# disk. The save is streamed to the cold bucket as ONE tar object (a raw save
# is a database's folder tree, often hundreds of thousands of files, which a
# file-by-file upload and listing could not finish inside the budget), and
# the object's size in the bucket must equal the bytes tar wrote before
# anything is deleted. The upload carries the CLI's own part checksums. The
# box copy is kept too; nothing here deletes a saved folder (Quote 25).
if [ -d "$OUT" ] && [ -n "$(find "$OUT" -type f -print -quit 2>/dev/null)" ]; then
  budget_check
  KEY="sebi-preserve/$(basename "$OUT").tar"
  TLOG=/run/tv-sebi-preserve-tar.log
  HINT=$(du -sb "$OUT" 2>/dev/null | cut -f1)
  case "$HINT" in ''|*[!0-9]*) HINT=0 ;; esac
  ( set -o pipefail; tar --totals -C "$OUT" -cf - . 2>"$TLOG" | timeout "$(left)" aws s3 cp --only-show-errors --region ap-south-1 --expected-size $((HINT + 1073741824)) - "s3://tv-prod-cold/$KEY" ) || sebi_abort "The saved tables could not be copied to the cloud bucket (s3://tv-prod-cold/$KEY)."
  TBYTES=$(sed -n 's/^Total bytes written: \([0-9]*\).*/\1/p' "$TLOG" | tail -n 1)
  RBYTES=$(timeout 60 aws s3api head-object --region ap-south-1 --bucket tv-prod-cold --key "$KEY" --query ContentLength --output text 2>/dev/null)
  case "$TBYTES" in ''|*[!0-9]*) sebi_abort 'The size of the cloud copy could not be read back from tar.' ;; esac
  [ "$TBYTES" = "$RBYTES" ] || sebi_abort "The cloud copy does not match the save (tar wrote $TBYTES bytes, the bucket holds ${RBYTES:-nothing})."
  echo "SEBI-PRESERVED-CLOUD $TBYTES bytes -> s3://tv-prod-cold/$KEY"
fi
lock_check"#,
    r#"docker ps -aq --filter volume=tv-questdb-data | xargs -r docker rm -f 2>/dev/null || true"#,
    r#"docker rm -f tv-questdb tv-loki tv-alloy 2>/dev/null || true"#,
    // 2026-09-27 (audit PR28): was `cd … || exit 0` followed by the compose
    // line. SSM runs this list as ONE script, so that `exit 0` ended the whole
    // action with the app stopped and disabled, and the 15-minute autopilot
    // reads a disabled unit as intentional. A missing compose folder now only
    // skips the compose step; the enable and restart below still run.
    r#"if cd /opt/tickvault/repo/deploy/docker; then docker compose down -v --remove-orphans || true; else echo 'DOCKER-RESET-NOTE: no compose folder, compose step skipped'; fi"#,
    r#"docker volume rm -f tv-questdb-data 2>/dev/null || true"#,
    r#"docker system prune -af --volumes || true"#,
    // 2026-09-27 (audit PR28): this failure exit brings the database
    // container back (the steps above removed it) and re-enables and restarts
    // the app first. Before, it left the unit disabled (see the compose note
    // above). The volume was NOT removed here, so the database comes back on
    // the data it already had.
    r#"if docker volume inspect tv-questdb-data >/dev/null 2>&1; then echo 'DOCKER-RESET-FAILED: tv-questdb-data still present (in-use) — NOT recreating to avoid re-attaching stale data. Holders:'; docker ps -a --filter volume=tv-questdb-data --format '{{.Names}} ({{.Status}})'; echo docker-reset-FAILED; /opt/tickvault/bin/tickvault ensure-questdb || true; systemctl enable tickvault || true; systemctl start tickvault || true; exit 1; fi"#,
    r#"echo 'OK: tv-questdb-data removed'"#,
    r#"rm -rf /opt/tickvault/data/instrument-cache /opt/tickvault/data/spill /opt/tickvault/data/dlq /opt/tickvault/data/ws_wal /opt/tickvault/data/groww 2>/dev/null || true"#,
    r#"rm -f /opt/tickvault/data/*/live-ticks.ndjson /opt/tickvault/data/*/*-status.json 2>/dev/null || true"#,
    r#"echo 'OK: host caches + feed capture/replay sources wiped (instrument-cache, spill, dlq, ws_wal, groww); logs preserved'"#,
    r#"/opt/tickvault/bin/tickvault ensure-questdb || true"#,
    r#"systemctl enable tickvault || true"#,
    r#"systemctl restart tickvault || true"#,
    r#"echo docker-reset-dispatched"#,
];

/// legacy: `lambda_handler docker-nuke-bare cmds` (handler.py:1338-1368) — captured from the RUNNING oracle.
pub const DOCKER_NUKE_BARE_COMMANDS: [&str; 13] = [
    r#"set +e"#,
    ON_BOX_LOCK_GUARD,
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
  echo "code=LAMBDA-PORTAL-01 SEBI-PRESERVE-ABORT: nothing was deleted; the database and the app are being restarted. Any partial save is left in $OUT."
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
# One reset at a time. A second run that finds the lock held stops before it
# touches anything and leaves the box to the run that holds it (exit 2, not
# sebi_abort: restarting the app here would fight the other run).
if command -v flock >/dev/null 2>&1; then
  exec 9>/run/tv-sebi-preserve.lock
  flock -n 9 || { echo 'SEBI-PRESERVE-BUSY: another database reset is already running; this one stopped before touching anything.'; exit 2; }
fi
# The whole save must finish inside the SSM command's default 3,600 s
# execution timeout, or SSM kills the script mid-copy and sebi_abort never
# runs. 45 minutes leaves the restore and the page room to run.
START=$(date +%s); BUDGET=2700
left() { echo $((START + BUDGET - $(date +%s))); }
budget_check() { [ "$(left)" -gt 60 ] || sebi_abort 'The save ran out of time (45 minutes) before it finished.'; }
# The console refuses this action 09:00-15:45 IST, but only when it is sent.
# A long save can carry the deletion past 09:00, so the box re-checks the
# same window (DATA_DESTRUCTIVE_LOCK_OPEN_SECS / _CLOSE_SECS, seconds of the
# IST day) right before the first step that deletes anything.
lock_check() {
  NOW=$(date -u +%s)
  case "$NOW" in ''|*[!0-9]*) sebi_abort 'The box clock could not be read to re-check the 09:00-15:45 IST lock.' ;; esac
  SOD=$(((NOW + 19800) % 86400))
  if [ "$SOD" -ge 32400 ] && [ "$SOD" -lt 56700 ]; then
    sebi_abort 'The save finished inside the 09:00-15:45 IST lock, so nothing was deleted. Run it again after 15:45.'
  fi
}
# Files and bytes under a directory, file contents only. `du -sb` also counts
# directory entries, whose size differs between the source and a fresh copy,
# so it cannot prove a copy complete.
fbytes() { find "$1" -type f -printf '%s\n' 2>/dev/null | awk '{s += $1; n++} END {print n + 0 ":" s + 0}'; }
# Everything that could be writing the volume: any running container that
# mounts it, plus the database container's own run state and start time. If
# this changes during a copy, something restarted the database (the 15-minute
# autopilot starts the database container when it finds it down) and the copy
# is of files that were moving.
qdb_quiet() { printf '%s|%s' "$(docker ps -q --filter volume=tv-questdb-data 2>/dev/null | tr -d '\n')" "$(docker inspect -f '{{.State.Running}} {{.State.StartedAt}}' tv-questdb 2>/dev/null)"; }
# The table folders of the 5-year tables on the volume (a WAL table's folder
# holds its unapplied WAL segments too), into DIRS.
sebi_dirs() {
  DIRS=''
  for t in $SEBI; do
    FOUND=0
    for d in "$MP/db/$t" "$MP/db/$t"~*; do
      [ -d "$d" ] || continue
      FOUND=1; DIRS="$DIRS $d"
    done
    [ "$FOUND" = 0 ] && echo "SEBI-ABSENT $t"
  done
}
# Free space must cover the save plus a fifth, and still leave 5 GiB for the
# app afterwards.
room_for() {
  FREE=$(df -B1 --output=avail "$1" 2>/dev/null | tail -n 1 | tr -d ' ')
  WANT=$(($2 + $2 / 5 + 5368709120))
  case "$FREE" in ''|*[!0-9]*) sebi_abort 'The free disk space could not be read.' ;; esac
  [ "$FREE" -gt "$WANT" ] || sebi_abort "Not enough free disk to save them (need $WANT bytes with margin, free $FREE)."
}
# How many 5-year WAL tables still hold rows QuestDB has not applied, or are
# suspended. `SELECT *` and `count()` both read only APPLIED rows, so an
# export of a table that is behind misses rows and its own check passes.
wal_behind() {
  WT=$(curl -fsS --max-time 15 --get --data-urlencode 'query=SELECT name, suspended, writerTxn, sequencerTxn FROM wal_tables()' "$QDB/exp" 2>/dev/null) || { echo unknown; return; }
  printf '%s\n' "$WT" | tail -n +2 | tr -d '"\r' | awk -F, -v s=" $SEBI " 'index(s, " " $1 " ") && ($2 == "true" || $3 != $4) {n++} END {print n + 0}'
}
# Stop the database so its files are quiescent, and record what is running
# against the volume so a restart during the copy is caught.
stop_qdb() {
  docker stop tv-questdb >/dev/null 2>&1 || true
  [ -z "$(docker ps -q --filter volume=tv-questdb-data 2>/dev/null)" ] || sebi_abort 'The database could not be stopped, so its files cannot be copied safely.'
  Q0=$(qdb_quiet)
}
# Copy the table folders in DIRS off the volume into $OUT/raw, each checked
# file for file and byte for byte against its source. The database must be
# stopped first (stop_qdb).
raw_copy() {
  # shellcheck disable=SC2086
  NEED=$(du -sbc $DIRS 2>/dev/null | tail -n 1 | cut -f1)
  case "$NEED" in ''|*[!0-9]*) sebi_abort 'The size of the tables to save could not be read.' ;; esac
  mkdir -p "$OUT/raw" || sebi_abort 'The save folder could not be created.'
  room_for "$OUT/raw" "$NEED"
  # The name-to-folder registry, so a restore can map the folders back.
  for f in "$MP/db"/tables.d* "$MP/db"/_tab_index.d; do
    [ -f "$f" ] || continue
    cp -a "$f" "$OUT/raw/" || sebi_abort 'The table registry could not be copied off the volume.'
  done
  for d in $DIRS; do
    budget_check
    [ "$(qdb_quiet)" = "$Q0" ] || sebi_abort 'Something restarted the database during the copy, so the copied files may be incomplete.'
    n=$(basename "$d")
    if timeout "$(left)" cp -a "$d" "$OUT/raw/$n" && [ "$(fbytes "$d")" = "$(fbytes "$OUT/raw/$n")" ]; then
      echo "SEBI-PRESERVED-RAW $n $(fbytes "$OUT/raw/$n") files:bytes -> $OUT/raw/$n"
    else
      echo "SEBI-PRESERVE-FAILED $n"
      sebi_abort "Copying the table $n off the volume failed or came out different."
    fi
  done
  [ "$(qdb_quiet)" = "$Q0" ] || sebi_abort 'Something restarted the database during the copy, so the copied files may be incomplete.'
}
MODE=export; EXPORTED=''
if [ -z "$ALL" ]; then
  MODE=raw
  echo 'SEBI-PRESERVE-UNAVAILABLE: QuestDB did not answer — copying the 5-year tables straight off the database volume instead.'
else
  i=0; BEHIND=$(wal_behind)
  while [ "$BEHIND" != 0 ] && [ "$i" -lt 24 ]; do sleep 5; i=$((i + 1)); BEHIND=$(wal_behind); done
  if [ "$BEHIND" != 0 ]; then
    MODE=raw
    echo "SEBI-PRESERVE-WAL-BEHIND: $BEHIND of the 5-year tables still have rows QuestDB has not applied (or it could not say), so an export would miss them — copying them straight off the database volume instead."
  fi
fi
if [ "$MODE" = raw ]; then
  VOLS=$(docker volume ls -q 2>/dev/null) || sebi_abort 'Docker did not answer, so the database volume could not be checked.'
  if printf '%s\n' "$VOLS" | grep -qx tv-questdb-data; then
    stop_qdb
    MP=$(docker volume inspect -f '{{.Mountpoint}}' tv-questdb-data 2>/dev/null)
    [ -n "$MP" ] && [ -d "$MP/db" ] || sebi_abort 'The database files could not be found on the volume.'
    sebi_dirs
    if [ -n "$DIRS" ]; then
      raw_copy
    fi
  else
    echo 'SEBI-VOLUME-ABSENT: there is no database volume, so there is nothing to lose.'
  fi
else
  mkdir -p "$OUT" || sebi_abort 'The save folder could not be created.'
  # Size the export from the tables' own folders when the volume can be read;
  # otherwise only the 5 GiB floor applies.
  EST=0
  MP=$(docker volume inspect -f '{{.Mountpoint}}' tv-questdb-data 2>/dev/null)
  if [ -n "$MP" ] && [ -d "$MP/db" ]; then
    sebi_dirs >/dev/null
    # shellcheck disable=SC2086
    [ -n "$DIRS" ] && EST=$(du -sbc $DIRS 2>/dev/null | tail -n 1 | cut -f1)
    case "$EST" in ''|*[!0-9]*) EST=0 ;; esac
  fi
  room_for "$OUT" "$EST"
  for t in $SEBI; do
    if printf '%s\n' "$ALL" | grep -qx "$t"; then
      budget_check
      if curl -fsS --max-time 300 --get --data-urlencode "query=SELECT * FROM $t" "$QDB/exp" -o "$OUT/$t.csv"; then
        # Verified, not assumed: the file must hold at least as many rows as
        # the table (header excluded). A quoted newline inside a value can only
        # ADD lines, so a short file is a truncated export and never passes.
        # The WAL check above is what makes count() the whole table.
        ROWS=$(curl -fsS --max-time 30 --get --data-urlencode "query=SELECT count() FROM $t" "$QDB/exp" 2>/dev/null | tail -n 1 | tr -d '"\r ')
        LINES=$(awk 'END{print NR}' "$OUT/$t.csv")
        case "$ROWS" in ''|*[!0-9]*) echo "SEBI-PRESERVE-FAILED $t"; sebi_abort "The row count of $t could not be read to check its export." ;; esac
        if [ "$LINES" -ge 1 ] && [ $((LINES - 1)) -ge "$ROWS" ]; then
          echo "SEBI-PRESERVED $t $ROWS rows $(wc -c <"$OUT/$t.csv" | tr -d ' ') bytes -> $OUT/$t.csv"
          EXPORTED="$EXPORTED $t"
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
  # 2026-09-27 (audit PR28): the table list above came from ONE bounded
  # query. A list cut short (a timeout mid-body, a partial reply), a table
  # whose metadata QuestDB could not load, or a folder left under a table's
  # old name after a rename all made a kept table read as absent, and the
  # volume holding it was then deleted. Every folder of a kept table that was
  # not exported is now copied off the volume as it is, the same way the
  # raw mode copies it. When the volume exists but its folders cannot be
  # read, nothing can be proven, so that stops the action.
  if [ -n "$MP" ] && [ -d "$MP/db" ]; then
    DIRS=''
    for t in $SEBI; do
      case " $EXPORTED " in *" $t "*) continue ;; esac
      for d in "$MP/db/$t" "$MP/db/$t"~*; do
        [ -d "$d" ] && DIRS="$DIRS $d"
      done
    done
    if [ -n "$DIRS" ]; then
      echo "SEBI-PRESERVE-UNLISTED:$DIRS are folders of 5-year tables that QuestDB did not list, so they were not exported. Copying them straight off the database volume."
      stop_qdb
      raw_copy
    fi
  elif docker volume inspect tv-questdb-data >/dev/null 2>&1; then
    sebi_abort 'The database volume exists but its folders could not be read, so it cannot be proven that every 5-year table was saved.'
  fi
fi
# 2026-09-27 (audit PR28): a copy on this box's own disk is lost with the
# disk. The save is streamed to the cold bucket as ONE tar object (a raw save
# is a database's folder tree, often hundreds of thousands of files, which a
# file-by-file upload and listing could not finish inside the budget), and
# the object's size in the bucket must equal the bytes tar wrote before
# anything is deleted. The upload carries the CLI's own part checksums. The
# box copy is kept too; nothing here deletes a saved folder (Quote 25).
if [ -d "$OUT" ] && [ -n "$(find "$OUT" -type f -print -quit 2>/dev/null)" ]; then
  budget_check
  KEY="sebi-preserve/$(basename "$OUT").tar"
  TLOG=/run/tv-sebi-preserve-tar.log
  HINT=$(du -sb "$OUT" 2>/dev/null | cut -f1)
  case "$HINT" in ''|*[!0-9]*) HINT=0 ;; esac
  ( set -o pipefail; tar --totals -C "$OUT" -cf - . 2>"$TLOG" | timeout "$(left)" aws s3 cp --only-show-errors --region ap-south-1 --expected-size $((HINT + 1073741824)) - "s3://tv-prod-cold/$KEY" ) || sebi_abort "The saved tables could not be copied to the cloud bucket (s3://tv-prod-cold/$KEY)."
  TBYTES=$(sed -n 's/^Total bytes written: \([0-9]*\).*/\1/p' "$TLOG" | tail -n 1)
  RBYTES=$(timeout 60 aws s3api head-object --region ap-south-1 --bucket tv-prod-cold --key "$KEY" --query ContentLength --output text 2>/dev/null)
  case "$TBYTES" in ''|*[!0-9]*) sebi_abort 'The size of the cloud copy could not be read back from tar.' ;; esac
  [ "$TBYTES" = "$RBYTES" ] || sebi_abort "The cloud copy does not match the save (tar wrote $TBYTES bytes, the bucket holds ${RBYTES:-nothing})."
  echo "SEBI-PRESERVED-CLOUD $TBYTES bytes -> s3://tv-prod-cold/$KEY"
fi
lock_check"#,
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
