#!/usr/bin/env bash
# Read-only diagnostics for the 2026-08-24 write-amplification + tick-refusal findings.
#
# Settles the open questions in `.claude/plans/active-plan-feed-hardening.md` Items 12-14.
# Every query here is a SELECT or a CloudWatch read. It mutates NOTHING: no DDL, no config,
# no constant, no instance state. Safe to run mid-session.
#
# Usage:  bash scripts/diagnose-write-amplification.sh [YYYY-MM-DD]
#         (defaults to today IST)
set -uo pipefail

QDB="${TV_QDB_URL:-http://127.0.0.1:9000}"
DAY="${1:-$(TZ=Asia/Kolkata date +%F)}"
VOL="${TV_ROOT_VOLUME_ID:-vol-0c6ab6e593e39d8c8}"

q() { # q <label> <sql>
  printf '\n--- %s ---\n' "$1"
  curl -s -m 30 -G "$QDB/exec" --data-urlencode "query=$2" \
    | sed -e 's/{"query".*"dataset"://' -e 's/,"count".*//' | head -c 2000
  printf '\n'
}

echo "=============================================================="
echo " tickvault write-amplification diagnostics"
echo " trading date : $DAY"
echo " questdb      : $QDB"
echo " root volume  : $VOL"
echo "=============================================================="

if ! curl -s -m 5 "$QDB/exec?query=SELECT%201" >/dev/null 2>&1; then
  echo "QuestDB not reachable at $QDB — run this ON THE BOX during a session."
  echo "Skipping the SQL half; the CloudWatch half below still works."
else
  echo
  echo "########## E1 — is ticks.ts out-of-order? (Item 12, Candidate B) ##########"
  echo "# ticks.ts is stamped with the exchange LAST-TRADE time, not receipt. If a large"
  echo "# share lands more than an hour behind receipt, rows are landing in ALREADY-CLOSED"
  echo "# hourly partitions and forcing merge-rewrites. That is the amplifier."
  q "total ticks today" \
    "SELECT count() FROM ticks WHERE ts >= '${DAY}T00:00:00.000000Z'"
  q "ticks landing >1h behind receipt (the smoking gun)" \
    "SELECT count() FROM ticks WHERE ts >= '${DAY}T00:00:00.000000Z' AND ts < dateadd('h',-1,received_at)"
  q "lateness distribution (minutes behind receipt)" \
    "SELECT datediff('m', ts, received_at) AS late_min, count() FROM ticks WHERE ts >= '${DAY}T00:00:00.000000Z' GROUP BY late_min ORDER BY late_min DESC LIMIT 20"

  echo
  echo "########## E3 — are the refused ticks the indices? (Item 12b) ##########"
  echo "# 17,931 ticks/session are refused on an absolute timestamp bound. If IDX_I rows"
  echo "# are ABSENT from ticks while indices are subscribed, the index hypothesis holds."
  q "rows per segment today" \
    "SELECT segment, count() FROM ticks WHERE ts >= '${DAY}T00:00:00.000000Z' GROUP BY segment"
  q "distinct instruments per segment" \
    "SELECT segment, count_distinct(security_id) FROM ticks WHERE ts >= '${DAY}T00:00:00.000000Z' GROUP BY segment"

  echo
  echo "########## Storage shape — which table dominates? (Item 12, Candidate A) ##########"
  q "table sizes / partition counts" \
    "SELECT table_name, partitionBy, walEnabled FROM tables() ORDER BY table_name"
  q "candle rows today across timeframes" \
    "SELECT count() FROM candles_1s WHERE ts >= '${DAY}T00:00:00.000000Z'"
  q "market_depth rows today" \
    "SELECT count() FROM market_depth WHERE ts >= '${DAY}T00:00:00.000000Z'"
  q "WAL apply backlog (writerTxn lag = unapplied commits)" \
    "SELECT name, sequencerTxn, writerTxn, sequencerTxn - writerTxn AS lag FROM wal_tables() ORDER BY lag DESC LIMIT 15"
fi

echo
echo "########## Disk traffic vs logical rows (the 25x question) ##########"
if command -v aws >/dev/null 2>&1; then
  for M in VolumeWriteBytes VolumeReadBytes; do
    T=$(aws cloudwatch get-metric-statistics --namespace AWS/EBS --metric-name "$M" \
        --dimensions "Name=VolumeId,Value=$VOL" \
        --start-time "${DAY}T03:00:00Z" --end-time "${DAY}T12:00:00Z" \
        --period 300 --statistics Sum --output text 2>/dev/null \
        | grep DATAPOINTS | awk '{s+=$2} END{printf "%.1f", s/1073741824}')
    echo "  $M session total : ${T:-n/a} GB"
  done
  echo "  (compare against the logical row totals above; ratio >> 1 is the amplification)"
else
  echo "  aws CLI absent — run scripts/ensure-aws-cli.sh"
fi

echo
echo "########## Verdict guide ##########"
cat <<'GUIDE'
  E1 late-tick count LARGE  -> Candidate B (out-of-order into closed partitions).
                               Fix is the designated timestamp, NOT the flush cadence.
                               NOTE: this makes Item 14 decoupling HARMFUL until capped
                               (wider batches = wider ts span = more merge work).
  E1 late-tick count ~ZERO  -> Candidate B exonerated. Suspect commit COUNT (candle path,
                               100ms bare timer, 24 tables, PARTITION BY DAY) or the
                               16 MiB QDB_CAIRO_WRITER_DATA_APPEND_PAGE_SIZE.
  IDX_I absent from ticks   -> the 17,931 refusals are the indices (LTT=0 on a computed
                               index). Fix is a counted, row-REFUSED sentinel (Item 12b).
  IDX_I present and ticking -> index hypothesis DEAD; decode a WAL segment for real values.
  WAL lag growing           -> QuestDB apply cannot keep up; ingest exceeds its envelope.
GUIDE
echo
echo "Done. Nothing was modified."
