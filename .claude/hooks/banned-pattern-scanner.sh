#!/bin/bash
# banned-pattern-scanner.sh — Scans staged .rs files for ALL banned patterns
# Called by pre-commit-gate.sh. Exit 2 = block commit.
# This is Layer 1 enforcement: mechanical, zero LLM involvement.

set -euo pipefail

PROJECT_DIR="${1:-.}"
STAGED_FILES="${2:-}"

# If no staged files passed, detect them
if [ -z "$STAGED_FILES" ]; then
  STAGED_FILES=$(cd "$PROJECT_DIR" && git diff --cached --name-only --diff-filter=ACMR 2>/dev/null | grep '\.rs$' || true)
fi

if [ -z "$STAGED_FILES" ]; then
  exit 0
fi

VIOLATIONS=0
REPORT=""

# Helper: extract ONLY production code from a Rust file (strips #[cfg(test)] modules and test functions)
# Uses awk to track brace depth inside #[cfg(test)] and #[test] blocks
extract_prod_code() {
  local file="$1"
  awk '
    BEGIN { skip=0; depth=0; exempt=0; skip_next=0; buf="" }
    # Skip #[cfg(test)] and #[test] blocks (brace-depth tracking)
    # Only trigger when NOT already inside a skip block to prevent
    # nested #[test] attributes from resetting the outer module depth.
    skip==0 && /^[[:space:]]*#\[cfg\(test\)\]/ { skip=1; depth=0; next }
    skip==0 && /^[[:space:]]*#\[test\]/ { skip=1; depth=0; next }
    # Inside a skip block that has entered braces — track depth
    skip==1 && depth > 0 {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0) { skip=0; depth=0 }
      next
    }
    # Inside a skip block, first line with opening brace — start depth tracking
    skip==1 && /\{/ {
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth <= 0) { skip=0; depth=0 }
      next
    }
    # Inside a skip block, no braces yet (attribute lines like #[allow()])
    skip==1 { next }
    # Block-level exemptions: O(1) EXEMPT: begin ... O(1) EXEMPT: end
    /O\(1\) EXEMPT: begin/ { exempt=1; next }
    /O\(1\) EXEMPT: end/ { exempt=0; next }
    exempt==1 { next }
    # Standalone APPROVED or O(1) EXEMPT comment lines handle two cases:
    # 1. Comment AFTER violation (rustfmt moves #[allow()] inline comments down):
    #    If the buffered line is a #[allow()] -> retroactively approve it
    # 2. Comment BEFORE violation: approve the NEXT line (skip_next=1)
    /^[[:space:]]*\/\/.*APPROVED:/ || /^[[:space:]]*\/\/.*O\(1\) EXEMPT:/ {
      if (buf != "" && buf ~ /#\[allow\(/) { buf = ""; next }
      if (buf != "") print buf
      buf = ""
      skip_next = 1
      next
    }
    # If previous standalone approval comment said skip next line
    skip_next > 0 { skip_next = 0; next }
    # Buffer each code line to allow retroactive approval from following comment
    {
      if (buf != "") print buf
      buf = NR": "$0
    }
    END { if (buf != "") print buf }
  ' "$file"
}

# Helper: scan staged files for a pattern, excluding test files and test modules
scan_prod_code() {
  local pattern="$1"
  local description="$2"
  local files="$3"

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue

    # Skip test files entirely.
    # 2026-07-05: also skip examples/ and src/bin/ — non-production binaries
    # (proof examples + operator CLIs like tv_doctor whose OUTPUT is stdout).
    # This mirrors the compile-time policy: the print/unwrap deny lints are
    # scoped to each crate's prod lib.rs, not to examples/bins.
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/|/examples/|/src/bin/)'; then
      continue
    fi

    # Extract prod code (strips #[cfg(test)] blocks), then scan
    local matches
    matches=$(extract_prod_code "$full_path" \
      | grep "$pattern" \
      | grep -v '// test' \
      | grep -v '// TODO' \
      | grep -v '// SAFETY:' \
      | grep -v '// APPROVED:' \
      | grep -v '// O(1) EXEMPT:' \
      | grep -v '/// ' \
      | grep -v '//!' \
      || true)

    if [ -n "$matches" ]; then
      REPORT="${REPORT}\n  [BANNED] $description in $file:"
      while IFS= read -r match; do
        REPORT="${REPORT}\n    $match"
        VIOLATIONS=$((VIOLATIONS + 1))
      done <<< "$matches"
    fi
  done <<< "$files"
}

# Helper: scan ONLY hot-path code
#
# 2026-07-05 REAL-PATH FIX: the previous filter referenced crates that DO NOT
# EXIST (`crates/websocket/`, `crates/oms/`) and a core module that does not
# exist (`crates/core/src/ticker/`), so the actual per-tick chain (parser,
# tick pipeline, tick persistence, WAL frame spill, groww bridge) was NEVER
# scanned. The canonical hot-path file set is now shared with
# dedup-latency-scanner.sh and self-tested by
# .claude/hooks/hot-path-scanner-selftest.sh (fails if any configured path
# matches zero existing files — a rename can never blind the scanner again).
#
# Hot path (the REAL per-tick chain, enumerated from the current tree):
#   crates/core/src/parser/                 — binary packet parsing (per frame)
#   crates/core/src/pipeline/               — tick processor + enricher + guards (per tick)
#   crates/core/src/websocket/              — WS read loop + subscription path
#   crates/trading/                         — candles aggregator + indicator + strategy + risk + in_mem
#   crates/storage/src/ws_frame_spill.rs    — per-frame WAL append
# (crates/app/src/groww_bridge.rs — the per-tick Groww NDJSON consume path — was
#  DELETED 2026-07-15 with the Groww live feed; its alternative is removed below.)
# (crates/storage/src/tick_persistence.rs + tick_row_builder.rs — the per-tick
#  ILP append/row-build chain — were DELETED 2026-07-17 in the stage-2 dead-WS
#  sweep (PR #1631); their alternatives are removed below per the zero-match
#  guard in hot-path-scanner-selftest.sh.)
# Cold path within core (auth/, instrument/, notification/, historical/) is NOT hot path.
# Cold path within trading (oms/) — order placement is network-I/O-bound, not
# per-tick latency-critical (pre-existing documented exclusion, kept).
# NOTE: keep this regex a flat '|'-separated list of alternatives with NO
# nested groups — hot-path-scanner-selftest.sh splits it on '|' to verify
# every alternative matches at least one real file.
HOT_PATH_INCLUDE_REGEX='^crates/core/src/parser/|^crates/core/src/pipeline/|^crates/core/src/websocket/|^crates/trading/|^crates/storage/src/ws_frame_spill\.rs$'
HOT_PATH_EXCLUDE_REGEX='^crates/trading/src/oms/'

scan_hot_path() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local hot_path_files

  hot_path_files=$(echo "$files" | grep -E "$HOT_PATH_INCLUDE_REGEX" | grep -vE "$HOT_PATH_EXCLUDE_REGEX" || true)
  if [ -z "$hot_path_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$description" "$hot_path_files"
}

echo "=== Banned Pattern Scanner ===" >&2

# ─────────────────────────────────────────────
# CATEGORY 1: Universal bans (all prod code)
# ─────────────────────────────────────────────

# .unwrap() in production code
scan_prod_code '\.unwrap()' '.unwrap() — use ? with anyhow/thiserror' "$STAGED_FILES"

# .expect() in production code (same as .unwrap with a message)
scan_prod_code '\.expect(' '.expect() — use ? with anyhow/thiserror' "$STAGED_FILES"

# println! / dbg! / eprintln! in production code
scan_prod_code 'println!' 'println! — use tracing macros' "$STAGED_FILES"
scan_prod_code 'dbg!' 'dbg! — use tracing macros' "$STAGED_FILES"
scan_prod_code 'eprintln!' 'eprintln! — use tracing macros' "$STAGED_FILES"

# localhost / 127.0.0.1 in application code (not config/test)
scan_prod_code '"localhost' 'localhost — use Docker DNS hostnames' "$STAGED_FILES"
scan_prod_code '"127\.0\.0\.1' '127.0.0.1 — use Docker DNS hostnames' "$STAGED_FILES"

# DashMap anywhere
scan_prod_code 'DashMap' 'DashMap — use papaya for concurrent maps' "$STAGED_FILES"

# Unbounded channels
scan_prod_code 'unbounded_channel\|unbounded()' 'unbounded channel — use bounded capacity always' "$STAGED_FILES"
scan_prod_code 'mpsc::channel()' 'mpsc::channel() without capacity — use bounded' "$STAGED_FILES"

# bincode (banned, use bitcode)
scan_prod_code 'bincode::' 'bincode — use bitcode instead' "$STAGED_FILES"

# #[allow(...)] without approval
scan_prod_code '#\[allow(' '#[allow()] — requires // APPROVED: comment on same/preceding line' "$STAGED_FILES"

# unsafe blocks
scan_prod_code 'unsafe {' 'unsafe block — requires // SAFETY: justification' "$STAGED_FILES"
scan_prod_code 'unsafe fn ' 'unsafe fn — requires // SAFETY: justification' "$STAGED_FILES"

# Banned infrastructure (use alternatives)
scan_prod_code 'promtail' 'Promtail — use Grafana Alloy' "$STAGED_FILES"
scan_prod_code 'jaeger' 'Jaeger v1 — use Jaeger v2 or OTLP' "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 2: Hot-path only bans (core/trading/websocket/oms)
# ─────────────────────────────────────────────

# .clone() on hot path
scan_hot_path '\.clone()' '.clone() on hot path — use Copy types or references' "$STAGED_FILES"

# Box::new / Vec::new / String::new allocations on hot path
scan_hot_path 'Box::new(' 'Box::new() on hot path — zero allocation required' "$STAGED_FILES"
scan_hot_path 'Vec::new()' 'Vec::new() on hot path — pre-allocate or use ArrayVec' "$STAGED_FILES"
scan_hot_path 'vec!\[' 'vec![] on hot path — pre-allocate or use ArrayVec' "$STAGED_FILES"
scan_hot_path 'String::new()' 'String::new() on hot path — use &str or pre-allocated' "$STAGED_FILES"
scan_hot_path 'String::from(' 'String::from() on hot path — use &str or pre-allocated' "$STAGED_FILES"
scan_hot_path '\.to_string()' '.to_string() on hot path — use &str or pre-allocated' "$STAGED_FILES"
scan_hot_path '\.to_owned()' '.to_owned() on hot path — use references' "$STAGED_FILES"
scan_hot_path 'format!' 'format!() on hot path — zero allocation required' "$STAGED_FILES"

# .collect() on hot path (unbounded allocation)
scan_hot_path '\.collect()' '.collect() on hot path — unbounded allocation' "$STAGED_FILES"

# dyn Trait on hot path (use enum_dispatch)
scan_hot_path 'dyn ' 'dyn Trait on hot path — use enum_dispatch' "$STAGED_FILES"

# HashMap::new() without capacity on hot path
scan_hot_path 'HashMap::new()' 'HashMap::new() on hot path — use with_capacity()' "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 2b: Storage crate — price precision
# ─────────────────────────────────────────────

# f64::from() on f32 prices in storage crate — must use f32_to_f64_clean()
# Raw f64::from(f32) widens IEEE 754 bit pattern, causing 10.20 → 10.19999980926514
scan_storage_precision() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local storage_files

  storage_files=$(echo "$files" | grep -E '^crates/storage/' || true)
  if [ -z "$storage_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$description" "$storage_files"
}

# 2026-05-25: extended-scope scanner for the ParsedTick price fields.
# Operator-spotted candles_1m corruption (23937.30078125) traced to
# f64::from(tick.last_traded_price) in crates/trading and crates/core.
# Those crates have legitimate u32→f64 / u16→f64 usages so we can't
# ban bare `f64::from(` like we do in storage — we target the specific
# f32 price fields by name.
scan_tick_price_precision() {
  local pattern="$1"
  local files="$2"
  local price_path_files

  price_path_files=$(echo "$files" | grep -E '^crates/(storage|trading|core)/' || true)
  if [ -z "$price_path_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$pattern — use tickvault_common::price_precision::f32_to_f64_clean() (data-integrity.md \"Price Precision Preservation\")" "$price_path_files"
}

scan_storage_precision 'f64::from(' 'f64::from(f32) in storage — use f32_to_f64_clean() to preserve Dhan price precision' "$STAGED_FILES"
scan_tick_price_precision 'f64::from(tick\.last_traded_price)' "$STAGED_FILES"
scan_tick_price_precision 'f64::from(tick\.day_open)'           "$STAGED_FILES"
scan_tick_price_precision 'f64::from(tick\.day_high)'           "$STAGED_FILES"
scan_tick_price_precision 'f64::from(tick\.day_low)'            "$STAGED_FILES"
scan_tick_price_precision 'f64::from(tick\.day_close)'          "$STAGED_FILES"
scan_tick_price_precision 'f64::from(tick\.average_traded_price)' "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 3: Hardcoded values (all prod code)
# ─────────────────────────────────────────────

# Hardcoded Duration::from_secs with literal number (digits-only inside parens)
scan_prod_code 'Duration::from_secs([0-9][0-9]*)' 'Hardcoded Duration — use named constant' "$STAGED_FILES"
scan_prod_code 'Duration::from_millis([0-9][0-9]*)' 'Hardcoded Duration — use named constant' "$STAGED_FILES"
scan_prod_code 'Duration::from_nanos([0-9][0-9]*)' 'Hardcoded Duration — use named constant' "$STAGED_FILES"

# Hardcoded port numbers
scan_prod_code '":[0-9][0-9][0-9][0-9]"' 'Hardcoded port — use config' "$STAGED_FILES"

# Hardcoded WebSocket/API URLs (exclude protocol validation like starts_with("https://"))
scan_prod_code '"wss://[a-zA-Z]' 'Hardcoded WebSocket URL — use config' "$STAGED_FILES"
scan_prod_code '"https://[a-zA-Z]' 'Hardcoded HTTPS URL — use config' "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 4: Dhan LOCKED FACTS (Tickets #5519522, #5525125)
# ─────────────────────────────────────────────
#
# These patterns cannot appear in production code. Each one represents a
# known-broken configuration that Dhan support explicitly told us NOT to
# use. If you need to reintroduce one, open a new Dhan ticket first.

# 2026-04-23 REVERSAL of Ticket #5519522: Python SDK `dhanhq==2.2.0rc1`
# verified on our account that the ROOT path `/` is the working URL for
# 200-depth at SecurityId 72271. The `/twohundreddepth` path (what the
# ticket originally told us to use) caused Protocol(ResetWithoutClosingHandshake)
# in our Rust client for 2+ weeks. Ban the old path so no one regresses.
scan_prod_code '"wss://full-depth-api\.dhan\.co/twohundreddepth' \
  'Dhan LOCKED FACT (Python SDK 2026-04-23) — 200-depth MUST use ROOT path /, not /twohundreddepth' \
  "$STAGED_FILES"
scan_prod_code 'DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.*twohundreddepth' \
  'Dhan LOCKED FACT (2026-04-23) — DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL must be ROOT path, not /twohundreddepth' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 5: I-P1-11 segment-aware security_id uniqueness
# ─────────────────────────────────────────────
#
# Dhan reuses numeric security_id across different ExchangeSegment values
# (e.g. NIFTY IDX_I id=13 vs some NSE_EQ id=13). Any collection keyed on
# security_id ALONE silently drops one of the two — leading to missing
# WebSocket subscriptions, wrong tick enrichment, and silent data loss.
#
# Every HashSet<u32> / HashSet<SecurityId> / HashMap<u32, _> / HashMap<SecurityId, _>
# in instrument-handling paths must either:
#  (a) use the composite key (u32, ExchangeSegment) / (SecurityId, ExchangeSegment), OR
#  (b) carry a `// APPROVED: single-segment context — <reason>` comment
#      proving every value belongs to one known segment.
#
# Scope: paths where cross-segment instruments can appear.
scan_instrument_paths() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local instrument_files

  instrument_files=$(echo "$files" | grep -E \
    '^crates/(common|core|trading)/src/(instrument|pipeline|websocket|greeks)/|^crates/common/src/instrument_registry\.rs$' \
    || true)
  if [ -z "$instrument_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$description" "$instrument_files"
}

# Ban single-key collections on u32 / SecurityId — the exact bug class from
# Parthiban's 2026-04-17 live finding (NIFTY IDX_I id=13 dropped).
scan_instrument_paths 'HashSet<u32>' \
  'I-P1-11: HashSet<u32> — use HashSet<(u32, ExchangeSegment)> or add // APPROVED: comment' \
  "$STAGED_FILES"
scan_instrument_paths 'HashSet<SecurityId>' \
  'I-P1-11: HashSet<SecurityId> — use HashSet<(SecurityId, ExchangeSegment)> or add // APPROVED: comment' \
  "$STAGED_FILES"
scan_instrument_paths 'HashMap<u32,' \
  'I-P1-11: HashMap<u32, _> — use HashMap<(u32, ExchangeSegment), _> or add // APPROVED: comment' \
  "$STAGED_FILES"
scan_instrument_paths 'HashMap<SecurityId,' \
  'I-P1-11: HashMap<SecurityId, _> — use HashMap<(SecurityId, ExchangeSegment), _> or add // APPROVED: comment' \
  "$STAGED_FILES"

# Also ban legacy registry.get(id) / registry.contains(id) in production
# — use get_with_segment / contains_with_segment. Single-segment callers
# that genuinely cannot know the segment must add // APPROVED: comment.
scan_instrument_paths '\.registry\.get([^_]' \
  'I-P1-11: registry.get(id) — use registry.get_with_segment(id, segment) or add // APPROVED: comment' \
  "$STAGED_FILES"
scan_instrument_paths '\.registry\.contains([^_]' \
  'I-P1-11: registry.contains(id) — use registry.contains_with_segment(id, segment) or add // APPROVED: comment' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 6: LIVE-FEED PURITY (Parthiban directive 2026-04-17)
# ─────────────────────────────────────────────
#
# "nowhere the backfill should happen ... live market feed should contain
#  only live market feed data alone ... historical candle data fetch is a
#  separate functionality"
#
# Hard ban on synthesising ticks from historical candles and writing them
# into the `ticks` QuestDB table. The BackfillWorker module was DELETED
# on 2026-04-17; these patterns block anyone from re-introducing it.
#
# Rule: `ticks` table is populated EXCLUSIVELY by WebSocket-sourced
# `ParsedTick` structs flowing through `crates/core/src/pipeline/`.
# Historical candle data lives in `historical_candles` + `candles_*`
# materialized views and MUST NOT cross into `ticks`.

scan_live_feed_purity() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local target_files

  # Ban applies to the `historical/` module tree and any new REST backfill path.
  target_files=$(echo "$files" | grep -E \
    '^crates/core/src/historical/|^crates/(core|trading|app)/src/.*/(backfill|synth)' \
    || true)
  if [ -z "$target_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$description" "$target_files"
}

# Hard bans — these symbols MUST NOT appear inside any historical/ or
# backfill-named file. If they reappear, the backfill worker is being
# re-introduced and the commit is blocked.
scan_live_feed_purity 'TickPersistenceWriter' \
  'LIVE-FEED-PURITY: TickPersistenceWriter MUST NOT be used from historical/backfill paths (2026-04-17 directive). ticks table = live WS data only.' \
  "$STAGED_FILES"
scan_live_feed_purity 'append_tick\b' \
  'LIVE-FEED-PURITY: append_tick() MUST NOT be called from historical/backfill paths. Historical data belongs in historical_candles only.' \
  "$STAGED_FILES"
scan_live_feed_purity 'BackfillWorker\|run_backfill\|synthesize_ticks' \
  'LIVE-FEED-PURITY: BackfillWorker / synth-tick pipeline was DELETED 2026-04-17. Re-introducing it is forbidden by Parthiban directive.' \
  "$STAGED_FILES"

# Also ban the historical module re-exporting a `backfill` submodule.
scan_prod_code 'pub mod backfill' \
  'LIVE-FEED-PURITY: pub mod backfill is banned — the module was DELETED 2026-04-17.' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 7: HOT-PATH SYNC FILESYSTEM (Wave 1 Item 0.d/0.a/0.e)
# ─────────────────────────────────────────────
#
# Synchronous `std::fs::*` calls (write / rename / create_dir_all etc.)
# inside the hot-path tick processor or persistence layer block the
# tokio runtime and stall tick ingestion when disk is slow or paused
# (chaos-mode tests, host I/O glitches, full disk).
#
# Wave 1 Item 0.d hoisted `create_dir_all` to a one-shot boot init.
# Item 0.a moved the PrevClose JSON cache write to a dedicated tokio
# task fed by `tokio::sync::mpsc::channel(64)`. This category locks
# both fixes in: any future re-introduction of `std::fs::*` (or
# `BufWriter::<File>::new(...)`) into the hot-path files MUST carry
# a `// HOT-PATH-EXEMPT: <reason>` comment on the directly preceding
# line OR be inside a `tokio::task::spawn_blocking` or `tokio::fs::*`
# closure (those are async-safe).
#
# Hot-path files (start narrow — extend as the spill async wrapper
# Item 0.b lands in a follow-up):
#   - crates/core/src/pipeline/  (the canonical hot-path dir)
# 2026-07-17 (stage-2 dead-WS sweep, PR #1631): the original sole target
# `crates/core/src/pipeline/tick_processor.rs` was DELETED with the dead
# tick chain — a single-file target would leave this scan vacuous. The
# scope is re-pointed at the surviving pipeline dir (already the canonical
# hot-path dir in HOT_PATH_INCLUDE_REGEX; verified zero sync-fs hits at
# re-point time), preserving the Wave-1 no-sync-fs-on-hot-path guard.
# Test files / boot init / drain task internals are exempt
# because they run on the blocking pool or once-only at boot.

scan_hot_path_sync_fs() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local target_files

  target_files=$(echo "$files" | grep -E \
    '^crates/core/src/pipeline/' \
    || true)
  if [ -z "$target_files" ]; then
    return
  fi

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/)'; then
      continue
    fi
    # Production-only: strip #[cfg(test)] blocks via the same awk used
    # in the universal scanner.
    local prod_code
    prod_code=$(extract_prod_code "$full_path")
    [ -z "$prod_code" ] && continue
    # Strip Rust comment lines so prose mentions of the banned literal
    # (e.g. doc-comments explaining the rule) don't trip the scanner.
    # Format produced by `extract_prod_code` is `<line_num>: <code>` —
    # we want to drop entries whose code starts with `///`, `//!`, or `//`.
    local code_only
    code_only=$(echo "$prod_code" | grep -vE '^[0-9]+:\s*(///|//!|//)' || true)
    local hits
    hits=$(echo "$code_only" | grep -E "$pattern" || true)
    [ -z "$hits" ] && continue
    while IFS= read -r hit; do
      [ -z "$hit" ] && continue
      local hit_line_num
      hit_line_num=$(echo "$hit" | cut -d: -f1)
      [ -z "$hit_line_num" ] && continue
      # Look back up to 5 lines for HOT-PATH-EXEMPT — the marker may
      # be on the line above the offending call OR on a doc-comment
      # block above the enclosing fn declaration.
      local exempt=0
      local i
      for i in 1 2 3 4 5; do
        local back_line_num=$((hit_line_num - i))
        [ "$back_line_num" -lt 1 ] && break
        local back_text
        back_text=$(sed -n "${back_line_num}p" "$full_path" 2>/dev/null)
        if echo "$back_text" | grep -q 'HOT-PATH-EXEMPT:'; then
          exempt=1
          break
        fi
      done
      if [ "$exempt" = "1" ]; then
        continue
      fi
      VIOLATIONS=$((VIOLATIONS + 1))
      REPORT="${REPORT}\n  [BANNED] ${description} in ${file}:\n    ${hit}"
    done <<< "$hits"
  done <<< "$target_files"
}

scan_hot_path_sync_fs 'std::fs::write\b' \
  'Wave 1 Item 0.a: std::fs::write on hot path — use tokio::fs / spawn_blocking (prev_close_writer deleted 2026-07-17, stage-2 sweep) or add // HOT-PATH-EXEMPT: above the call' \
  "$STAGED_FILES"
scan_hot_path_sync_fs 'std::fs::rename\b' \
  'Wave 1 Item 0.a: std::fs::rename on hot path — owned by the async writer task or add // HOT-PATH-EXEMPT:' \
  "$STAGED_FILES"
scan_hot_path_sync_fs 'std::fs::create_dir_all\b' \
  'Wave 1 Item 0.d: std::fs::create_dir_all on hot path — hoist to init_prev_close_cache_dir() at boot or add // HOT-PATH-EXEMPT:' \
  "$STAGED_FILES"
scan_hot_path_sync_fs 'std::fs::create_dir\b' \
  'Wave 1 Item 0.d: std::fs::create_dir on hot path — hoist to boot init or add // HOT-PATH-EXEMPT:' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 8: MOVERS 22-TF — I-P1-11 in movers paths (Phase 13 of v3 plan)
# ─────────────────────────────────────────────
#
# Same I-P1-11 invariant as Cat 5 but scoped to the movers paths. The
# movers writers iterate every tick across every segment so single-segment
# collections keyed on security_id alone silently drop colliding entries.
scan_movers_paths() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local movers_files

  movers_files=$(echo "$files" | grep -E \
    '^(crates/(core|storage|common)/src/.*(mover|movers).*\.rs|crates/common/src/mover_types\.rs)$' \
    || true)
  if [ -z "$movers_files" ]; then
    return
  fi

  scan_prod_code "$pattern" "$description" "$movers_files"
}

scan_movers_paths 'HashSet<u32>' \
  'Cat 8 (Phase 13): HashSet<u32> in movers path — use HashSet<(u32, ExchangeSegment)> or add // APPROVED: single-segment context' \
  "$STAGED_FILES"
scan_movers_paths 'HashMap<u32,' \
  'Cat 8 (Phase 13): HashMap<u32, _> in movers path — use HashMap<(u32, ExchangeSegment), _> or add // APPROVED: single-segment context' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 9: MOVERS DDL REGISTRATION
# ─────────────────────────────────────────────
#
# Every `CREATE TABLE IF NOT EXISTS movers_` literal in a Rust source file
# MUST be emitted by the canonical helper in `movers_unified_persistence.rs`
# (the `movers_1s` base table + 24 materialized views). Hand-rolled DDL
# for orphan movers tables (e.g. `movers_42m`) skips the partition manager
# registration + S3 lifecycle config, so the partition rotation never
# happens and EBS fills silently.
scan_movers_orphan_ddl() {
  local files="$1"
  local rust_files
  rust_files=$(echo "$files" | grep -E '^crates/.+\.rs$' | grep -v '_test\.rs$' || true)
  [ -z "$rust_files" ] && return

  local orphans=""
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    [ ! -f "$file" ] && continue
    # Skip the canonical helper itself + tests. Use exact-path matches so
    # only the two known canonical storage modules are exempted (a rogue
    # `crates/api/src/movers_persistence.rs` would NOT bypass the check).
    case "$file" in
      crates/storage/src/movers_unified_persistence.rs) continue ;;
      crates/storage/src/movers_persistence.rs) continue ;;
      */tests/*) continue ;;
    esac
    # Find any literal CREATE TABLE IF NOT EXISTS movers_ outside the helper
    if grep -nE 'CREATE TABLE IF NOT EXISTS movers_' "$file" 2>/dev/null \
       | grep -v -- '// APPROVED:'; then
      orphans="${orphans}${file}\n"
    fi
  done <<< "$rust_files"

  if [ -n "$orphans" ]; then
    VIOLATIONS=$((VIOLATIONS + 1))
    REPORT="${REPORT}\n  [BANNED] Cat 9: Orphan movers DDL outside movers_unified_persistence.rs (or movers_persistence.rs for legacy stock/option/top tables) — use the canonical helper or add // APPROVED: comment.\n${orphans}"
  fi
}
scan_movers_orphan_ddl "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 10: RAM-FIRST HOT-PATH READS (Wave 7-A4, 2026-05-11)
# ─────────────────────────────────────────────
#
# Per `.claude/rules/project/aws-budget.md` § "RAM-First Architecture
# (Wave 7-A4 mandatory)":
#
# > Tick → strategy decision must read indicator state, today's sealed
# > bars, yesterday's sealed bars, and prev_day_OI cache from RAM only.
# > QuestDB is for: persistence, audit, cross-verify (cold path), boot
# > rehydration.
#
# Hard ban on QuestDB / SQL reads from the trading hot path. The
# strategy + indicator + risk-check code paths sit between the
# WebSocket tick parse and the order-out wire — every microsecond of
# DB I/O blocks the tokio worker and adds latency to the next order.
#
# The cold-path equivalents (boot rehydration, post-market cross-verify,
# operator dashboard SELECTs) are allowed in their respective modules.
#
# Hot-path files (start narrow — extend as Wave 7-A4 lands the full
# RAM-first migration):
#   - crates/trading/src/strategy/*.rs
#   - crates/trading/src/indicator/*.rs
#   - crates/trading/src/risk/*.rs
# 2026-07-17 (stage-2 dead-WS sweep, PR #1631) truth-sync: the
# `crates/core/src/pipeline/tick_processor.rs` alternative is removed
# (file DELETED with the dead tick chain), and the phantom
# `crates/trading/src/oms/risk_check.rs` (NEVER existed on main — the
# 2026-07-05 phantom-path bug class) is re-pointed at the REAL risk
# modules `crates/trading/src/risk/` (verified zero query hits at
# re-point time), matching the documented RAM-first intent
# (aws-budget.md: no SELECT in indicator/strategy/risk paths).
#
# Exempt: `// HOT-PATH-EXEMPT: <reason>` on the preceding line, or
# the SELECT/exec/query call lives inside a `tokio::task::spawn_blocking`
# or `tokio::task::spawn` closure (those run off the hot path).
#
# Wave 7-A4 status: this category lands the GUARD. The
# indicator/strategy migration to read from `BarCache` (W7-A4.3) ships
# in follow-up sub-PRs.

scan_ram_first_hot_path() {
  local pattern="$1"
  local description="$2"
  local files="$3"
  local target_files

  target_files=$(echo "$files" | grep -E \
    '^crates/trading/src/strategy/.*\.rs$|^crates/trading/src/indicator/.*\.rs$|^crates/trading/src/risk/.*\.rs$' \
    || true)
  if [ -z "$target_files" ]; then
    return
  fi

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    [ ! -f "$file" ] && continue

    # Find lines matching the pattern, excluding comment-only lines + tests
    local matches
    matches=$(grep -n -E "$pattern" "$file" 2>/dev/null \
      | grep -v -E '^[[:space:]]*//' \
      | grep -v -E '^[[:space:]]*#\[' \
      || true)

    [ -z "$matches" ] && continue

    while IFS= read -r match_line; do
      [ -z "$match_line" ] && continue
      local line_num
      line_num=$(echo "$match_line" | cut -d: -f1)
      local content
      content=$(echo "$match_line" | cut -d: -f2-)

      # Skip if inside a test module — naive check: any module above
      # this line containing `#[cfg(test)]` is exempt.
      if head -n "$line_num" "$file" | grep -qE '^[[:space:]]*#\[cfg\(test\)\]' && \
         head -n "$line_num" "$file" | tail -n 30 | grep -qE 'mod tests'; then
        continue
      fi

      # Allow `// HOT-PATH-EXEMPT:` comment on preceding line
      local prev_line
      prev_line=$(sed -n "$((line_num - 1))p" "$file" 2>/dev/null)
      if echo "$prev_line" | grep -qE '//\s*(HOT-PATH-EXEMPT|APPROVED):'; then
        continue
      fi

      # Allow if line is inside a spawn_blocking / tokio::spawn closure
      # (naive check: look backwards 20 lines for `spawn_blocking(` or
      # `tokio::spawn(`; if found and no closing `})` before our line,
      # we're inside the closure).
      local in_async_closure
      in_async_closure=$(head -n "$line_num" "$file" | tail -n 30 \
        | grep -cE 'spawn_blocking\s*\(|tokio::spawn\s*\(|tokio::task::spawn' \
        || true)
      # grep -c always prints a count; default to 0 if empty
      : "${in_async_closure:=0}"
      if [ "$in_async_closure" -gt 0 ] 2>/dev/null; then
        # heuristic — if a closure is open above and not closed yet,
        # we treat it as OK. Conservative: only skip if pattern is in
        # an obviously async context. The operator can always add
        # `// HOT-PATH-EXEMPT:` for clarity.
        :  # not skipping by default — operator must annotate
      fi

      VIOLATIONS=$((VIOLATIONS + 1))
      REPORT="${REPORT}\n  [BANNED] Cat 10 ($description): $file:$line_num\n    $content"
    done <<< "$matches"
  done <<< "$target_files"
}

# QuestDB / SQL read calls on the hot path — block any SELECT statement
# or low-level `.exec(` / `.query(` / `.fetch_*(` invocation.
scan_ram_first_hot_path '\bSELECT\b' \
  'RAM-FIRST: SELECT on hot path is banned (Wave 7-A4). Read from BarCache / IndicatorEngine RAM state.' \
  "$STAGED_FILES"
scan_ram_first_hot_path '\.exec\b[[:space:]]*\(' \
  'RAM-FIRST: .exec() on hot path is banned (Wave 7-A4). Use RAM cache or move to cold-path task.' \
  "$STAGED_FILES"
scan_ram_first_hot_path '\.query\b[[:space:]]*\(' \
  'RAM-FIRST: .query() on hot path is banned (Wave 7-A4). Use RAM cache or move to cold-path task.' \
  "$STAGED_FILES"
scan_ram_first_hot_path '\.fetch_(one|all|optional)\b[[:space:]]*\(' \
  'RAM-FIRST: .fetch_*() on hot path is banned (Wave 7-A4). Use RAM cache or move to cold-path task.' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# CATEGORY 11: G1 dual-gate market-hours rule (Phase 0 Item 11/12, 2026-05-17)
# ─────────────────────────────────────────────
#
# Per `topic-PHASE-0-LEAN-LOCKED.md` §8: the G1 exchange-gate decides
# "does this tick belong to the session?" — it MUST use the tick's
# `exchange_timestamp_ist`, NEVER the local wall-clock. The
# `is_within_market_hours_ist(now())` pattern was the root cause of
# the 2026-05-13 15:29:59.586 skipped-tick incident.
scan_prod_code 'is_within_market_hours[A-Za-z_]*[(][^)]*now[(][)]' \
  'G1-GATE: passing now() to is_within_market_hours_ist is the 2026-05-13 dual-gate bug (Phase 0 Item 11). Use tick.exchange_timestamp_ist for G1.' \
  "$STAGED_FILES"

# ─────────────────────────────────────────────
# RESULT
# ─────────────────────────────────────────────

if [ "$VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "BLOCKED: $VIOLATIONS banned pattern violation(s) found:" >&2
  echo -e "$REPORT" >&2
  echo "" >&2
  echo "Fix all violations before committing. See CLAUDE.md BANNED section." >&2
  exit 2
fi

echo "  Banned pattern scan: CLEAN ($( echo "$STAGED_FILES" | wc -l) files scanned)" >&2
exit 0
