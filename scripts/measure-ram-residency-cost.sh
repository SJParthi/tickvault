#!/usr/bin/env bash
# Measure — never estimate — what a full trading day of ticks, bars and depth
# would actually cost to hold in RAM.
#
# WHY THIS EXISTS. Every RAM figure this repository has quoted for full-day
# residency (12 GB, 42 GB, 175 GB) rests on two numbers that were never
# measured: ticks per day, and depth updates per second. The rule file's own
# tick figure is explicitly labelled "Assumed". An estimate stated with a unit
# reads exactly like a measurement, and that is how a guess becomes a plan.
#
# Everything below is READ-ONLY. It runs against a PAST session already in
# QuestDB, so it needs no live market — only the box to be up.
#
# Usage:  bash scripts/measure-ram-residency-cost.sh [YYYY-MM-DD]
#         (defaults to the most recent day that has rows)

set -euo pipefail

QDB="${QUESTDB_HTTP:-http://localhost:9000}"
DAY="${1:-}"

q() {
  # One /exec query. Prints the raw dataset row so the caller can read it.
  curl -sS --get "${QDB}/exec" --data-urlencode "query=$1" \
    | sed 's/.*"dataset":\[\(.*\)\],"count".*/\1/'
}

echo "=============================================================="
echo " RAM residency cost — MEASURED"
echo " QuestDB: ${QDB}"
echo "=============================================================="

if [ -z "$DAY" ]; then
  echo
  echo "-- Most recent day with tick rows -----------------------------"
  DAY=$(q "SELECT to_str(max(ts),'yyyy-MM-dd') FROM ticks" | tr -d '[]"')
  echo "   using: ${DAY}"
fi

FROM="${DAY}T00:00:00.000000Z"
TO="${DAY}T23:59:59.999999Z"

echo
echo "-- 1. TICKS: how many, how many instruments, what rate ---------"
echo "   (the number every RAM estimate hangs on)"
q "SELECT count() total_ticks,
          count_distinct(security_id) instruments,
          round(count() / 23100.0, 1) ticks_per_sec_avg,
          round(count() / 23100.0 / count_distinct(security_id), 4) per_instrument_per_sec
   FROM ticks
   WHERE ts >= '${FROM}' AND ts <= '${TO}'"

echo
echo "-- 2. TICK BURST: the busiest single second --------------------"
echo "   (a full-day average hides the open; the ring must survive the peak)"
q "SELECT max(n) peak_ticks_in_one_second
   FROM (SELECT ts, count() n FROM ticks
         WHERE ts >= '${FROM}' AND ts <= '${TO}'
         SAMPLE BY 1s)"

echo
echo "-- 3. SECOND BARS: sparse reality vs the dense assumption ------"
echo "   dense would be instruments x 23,100. Measured is what actually opened."
for tf in 1 5 10 15 30; do
  printf '   candles_s%-3s ' "$tf"
  q "SELECT count() bars, count_distinct(security_id) instruments
     FROM candles_s${tf}
     WHERE ts >= '${FROM}' AND ts <= '${TO}'" || echo "   (table absent)"
done

echo
echo "-- 4. MINUTE BARS ----------------------------------------------"
q "SELECT count() bars_1m, count_distinct(security_id) instruments
   FROM candles_1m
   WHERE ts >= '${FROM}' AND ts <= '${TO}' AND feed = 'dhan'"

echo
echo "-- 5. DEPTH: the dominant term, and the one never measured -----"
echo "   rows are per LEVEL per SIDE; a d5 book update is 10 rows, d20 is 40."
q "SELECT depth_kind,
          count() rows,
          count_distinct(security_id) instruments,
          round(count() / 23100.0, 1) rows_per_sec
   FROM market_depth
   WHERE ts >= '${FROM}' AND ts <= '${TO}'
   ORDER BY depth_kind" || echo "   (market_depth absent — depth never persisted)"

echo
echo "-- 6. DEPTH UPDATE RATE per instrument -------------------------"
echo "   books/sec/instrument = rows / (levels x 2) / seconds / instruments"
q "SELECT depth_kind,
          round(count() / 23100.0 / count_distinct(security_id), 3) rows_per_sec_per_instrument
   FROM market_depth
   WHERE ts >= '${FROM}' AND ts <= '${TO}'
   GROUP BY depth_kind" || true

echo
echo "=============================================================="
echo " HOW TO TURN THESE INTO RAM BYTES"
echo "--------------------------------------------------------------"
echo " ticks      : total_ticks        x 32 B   (compact record)"
echo " sec bars   : sum(section 3)     x 48 B   (RamBar, test-pinned)"
echo " min bars   : bars_1m + coarser  x 48 B"
echo " depth      : (rows / levels/2)  x 168 B  d5"
echo "                                 x 648 B  d20"
echo "                                 x 6400 B d200"
echo
echo " Then SCALE by 24600 / instruments — and state that scaling as an"
echo " assumption, because today's set is indices and cash stocks, which"
echo " tick MORE than the option strikes that would make up the 24,600."
echo " That makes the scaled figure an UPPER bound, not a forecast."
echo "=============================================================="
