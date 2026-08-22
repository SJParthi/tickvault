#!/usr/bin/env bash
# Count the REAL current-expiry option universe from the Dhan detailed master CSV.
#
# WHY THIS EXISTS: the proposed live universe (all NSE indices + NIFTY/BANKNIFTY
# futures & current-expiry options + NSE F&O stock futures & current-expiry
# options) has to fit inside MAX_DAILY_UNIVERSE_SIZE = 25,000. Every sizing
# estimate of it so far has rested on an ASSUMED "~220 stocks x ~51 strikes".
# Nobody has ever counted the real ladder. This counts it.
#
# Run ON THE BOX (the master is fetched daily there; egress is blocked from dev
# sandboxes). Read-only: parses a CSV, writes nothing, touches no service.
#
#   bash scripts/count-current-expiry-universe.sh [path-to-master.csv]
#
# Column names are resolved FROM THE HEADER, never by fixed position, because
# the vendor has reordered columns before.
set -euo pipefail

CSV="${1:-}"
if [[ -z "$CSV" ]]; then
  for c in /opt/tickvault/data/instrument-cache/api-scrip-master-detailed.csv \
           data/instrument-cache/api-scrip-master-detailed.csv \
           /tmp/api-scrip-master-detailed.csv; do
    [[ -f "$c" ]] && CSV="$c" && break
  done
fi
if [[ -z "$CSV" || ! -f "$CSV" ]]; then
  echo "master CSV not found. Pass the path explicitly, or fetch it first:" >&2
  echo "  curl -sSo /tmp/api-scrip-master-detailed.csv https://images.dhan.co/api-data/api-scrip-master-detailed.csv" >&2
  exit 2
fi

echo "master : $CSV"
echo "rows   : $(( $(wc -l < "$CSV") - 1 ))"
echo "today  : $(date +%Y-%m-%d)  (IST assumed to be the box timezone)"
echo

awk -F',' -v TODAY="$(date +%Y-%m-%d)" '
function col(name,   i){ for(i=1;i<=NF;i++){ if(toupper($i)==name) return i } return 0 }
NR==1{
  C_INSTR=col("INSTRUMENT_TYPE")
  if(C_INSTR==0){ C_INSTR=col("INSTRUMENT") }
  C_EXP=col("SM_EXPIRY_DATE")
  if(C_EXP==0){ C_EXP=col("EXPIRY_DATE") }
  C_UND=col("UNDERLYING_SYMBOL")
  C_EXCH=col("EXCH_ID")
  if(C_INSTR==0 || C_EXP==0 || C_UND==0){ print "HEADER PARSE FAILED"; exit 3 }
  next
}
{
  instr=$C_INSTR
  expd=substr($C_EXP,1,10)
  und=$C_UND
  exch=""
  if(C_EXCH>0){ exch=$C_EXCH }
  if(C_EXCH>0 && exch!="NSE"){ next }
  if(expd=="" || expd<TODAY){ next }

  if(instr=="OPTSTK"){
    if(!(und in stk_first)){ stk_first[und]=expd }
    else if(expd<stk_first[und]){ stk_first[und]=expd }
    stk_cnt[und "|" expd]++
  }
  else if(instr=="OPTIDX" && (und=="NIFTY" || und=="BANKNIFTY")){
    if(!(und in idx_first)){ idx_first[und]=expd }
    else if(expd<idx_first[und]){ idx_first[und]=expd }
    idx_cnt[und "|" expd]++
  }
  else if(instr=="FUTSTK"){ fut_stk[und "|" expd]=1 }
  else if(instr=="FUTIDX" && (und=="NIFTY" || und=="BANKNIFTY")){ fut_idx[und "|" expd]=1 }
}
END{
  n_stocks=0; stk_total=0; capped_total=0; min_l=999999; max_l=0; max_u="-"
  for(u in stk_first){
    c=stk_cnt[u "|" stk_first[u]]
    n_stocks++
    stk_total+=c
    if(c<min_l){ min_l=c }
    if(c>max_l){ max_l=c; max_u=u }
    if(c>102){ capped_total+=102 } else { capped_total+=c }
  }
  idx_total=0
  for(u in idx_first){ idx_total+=idx_cnt[u "|" idx_first[u]] }
  nf=0; for(k in fut_stk){ nf++ }
  ni=0; for(k in fut_idx){ ni++ }
  if(n_stocks==0){ print "no NSE OPTSTK rows found - wrong file or all expired"; exit 4 }

  printf "F&O STOCKS (NSE, live option ladder)          : %d\n", n_stocks
  printf "  current-expiry option contracts, FULL       : %d\n", stk_total
  printf "  average per stock                           : %.1f contracts (%.1f strikes)\n", stk_total/n_stocks, stk_total/n_stocks/2
  printf "  thinnest ladder                             : %d contracts\n", min_l
  printf "  deepest ladder                              : %d contracts (%s)\n", max_l, max_u
  printf "  SAME SET capped ATM +/- 25 (max 102/stock)  : %d\n\n", capped_total
  printf "NIFTY+BANKNIFTY current-expiry options        : %d\n", idx_total
  printf "NIFTY+BANKNIFTY futures, all expiries         : %d\n", ni
  printf "F&O stock futures, all expiries               : %d\n\n", nf

  other = idx_total + ni + nf + 119
  full_t = stk_total + other
  cap_t  = capped_total + other
  print  "== vs the 25,000 cap (119 = your measured NSE index count) =="
  if(full_t<=25000){ vf="FITS, " (25000-full_t) " spare" } else { vf="OVER by " (full_t-25000) }
  if(cap_t<=25000){  vc="FITS, " (25000-cap_t)  " spare" } else { vc="OVER by " (cap_t-25000) }
  printf "  FULL ladders : %d + %d = %d  -> %s\n", stk_total, other, full_t, vf
  printf "  ATM +/- 25   : %d + %d = %d  -> %s\n", capped_total, other, cap_t, vc
}' "$CSV"
