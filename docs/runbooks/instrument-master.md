# Runbook — Instrument master + Universe build failures

**When it fires:** `I-P0-01`..`I-P0-06`, `I-P1-01`..`I-P1-11`,
`I-P2-02`, `DATA-813` (invalid SecurityId), `InstrumentBuildFailed`,
`InstrumentRegistryCollisionDrift`.

**Consequence if not resolved:** WebSocket subscriptions fail →
no ticks → trading halted. Or worse, the app runs with a stale
universe and subscribes to expired contracts.

## Quick facts

- Source of truth: `https://images.dhan.co/api-data/api-scrip-master-detailed.csv`
- Download: daily at boot + at `instrument.daily_refresh_time` (config)
- Size: ~43MB, ~262k rows, ~158k after filter
- Composite key: `(security_id, exchange_segment)` — security_id
  alone is NOT unique (I-P1-11)
- F&O filter yields ~221 underlyings, ~106k derivative contracts
- Cache: rkyv binary, zero-copy deserialization on read

## Telegram alert → action (60-second version)

| Alert | First action |
|---|---|
| 🔴 `InstrumentBuildFailed` | See "Full rebuild" recovery below. Trading CANNOT start without instruments. |
| 🟡 `I-P1-11 InstrumentRegistryCrossSegmentCollision` | **Expected** noise — NIFTY id=13 + ABB, BANKNIFTY id=25 + ADANIENT. Silenced by triage rule. |
| 🟠 `InstrumentRegistryCollisionDrift` (count > 5) | New collision appeared — run manual rebuild to see which underlying Dhan reused. |
| 🔴 `I-P0-01 DuplicateSecurityId` | CSV has same (id, segment) twice — Dhan CSV corruption. Retry download. |
| 🔴 `I-P0-02 CountConsistency` (< 100 derivatives) | CSV truncated. Retry download. HARD FAIL — do not boot with truncated universe. |
| 🟠 `I-P0-04 CachePersistence` | Cache dir is on tmpfs — cache evaporates on restart. Move to persistent disk. |
| 🔴 `I-P0-06 EmergencyDownload` | All caches missing during market hours. CRITICAL — someone wiped `data/instrument-cache/`. |
| 🟠 `DATA-813 InvalidSecurityId` | Our code used a hardcoded id. Fix: always lookup from registry. Auto-fix: `scripts/auto-fix-refresh-instruments.sh`. |

## Root-cause checklist

### 1. Is the registry populated?

```bash
mcp__tickvault-logs__prometheus_query("tv_instrument_registry_total_entries")
# Expected: ~106000 after successful build
# < 10000 = truncated CSV or failed build
# 0 = app booted without universe (should NEVER happen — boot halts)
```

### 2. Is the CSV download working?

```bash
# Test the raw download (outside the app):
curl -sS -m 30 -o /tmp/scrip-master.csv \
  https://images.dhan.co/api-data/api-scrip-master-detailed.csv
wc -l /tmp/scrip-master.csv
# Expected: ~262000 lines. < 1000 = Dhan CDN returning an error page.
```

### 3. Last successful build timestamp

```bash
mcp__tickvault-logs__prometheus_query("time() - tv_instrument_universe_last_build_epoch_secs")
# Expected: < 86400 (within last 24h)
# > 172800 (48h) = daily refresh has been silently failing
```

### 4. Registry composite-index health

```bash
mcp__tickvault-logs__prometheus_query("tv_instrument_registry_cross_segment_collisions")
# Baseline: 2 (NIFTY/ABB, BANKNIFTY/ADANIENT). Other values need investigation.
```

## Recovery

### Manual rebuild via API

```bash
curl -X POST http://127.0.0.1:3001/api/instruments/rebuild
# This re-runs the full pipeline: download CSV -> parse -> filter ->
# build FnoUniverse -> persist to QuestDB -> reload registry.
# Expected duration: 30-60 seconds.
```

Or via the auto-fix script:

```bash
scripts/auto-fix-refresh-instruments.sh --dry-run
scripts/auto-fix-refresh-instruments.sh
# Logs to data/logs/auto-fix.log
```

### Full rebuild on boot failure

If `InstrumentBuildFailed` fires at boot, the app halts. Recovery:

```bash
# 1. Clear the cache (forces fresh CSV download)
rm -rf data/instrument-cache/*

# 2. Check disk space + network egress
df -h data/
curl -sS -m 5 https://images.dhan.co > /dev/null && echo "Dhan CDN reachable"

# 3. Restart
make stop && make run
```

If the app STILL fails to build, the fallback is the S3 backup
(if configured) — the universe builder falls back to the last-known-
good cached CSV from S3.

### I-P1-11 collision drift — investigating a new collision

```bash
# Identify which security_id now has multiple segments
# (QuestDB SQL):
SELECT security_id, ARRAY_AGG(DISTINCT exchange_segment) AS segments
FROM derivative_contracts
GROUP BY security_id
HAVING COUNT(DISTINCT exchange_segment) > 1;

# For each row: both entries are stored in composite_index.
# Code that uses `.registry.get(id)` gets ONE of them (race). Code
# that uses `.registry.get_with_segment(id, segment)` is correct.

# Grep for any legacy callers:
grep -rn "registry.get(" crates/ --include="*.rs" | grep -v "_with_segment"
# Result should be empty.
```

## Never do these

- **Never hardcode a SecurityId.** IDs change when instruments are
  relisted. Always lookup from `InstrumentRegistry`.
- **Never key a HashMap on `SecurityId` alone** in any path that
  crosses segments. Banned-pattern hook category 5 rejects the
  commit.
- **Never boot with < 100 derivatives.** I-P0-02 halts. Do NOT bypass.
- **Never use compact CSV for F&O.** Missing columns (no
  UNDERLYING_SECURITY_ID, no MTF_LEVERAGE). Always detailed.
- **Never parse `convertQty` as an integer** in position-convert API.
  Dhan sends string.

## Preventive measures

1. **Daily refresh** configured via
   `config/base.toml → instrument.daily_refresh_time`. Verify alive
   via `tv_instrument_universe_last_build_epoch_secs` (dashboard
   panel in `tv-data-lifecycle`).
2. **S3 backup** enabled — last-known-good CSV survives CDN outages.
3. **Composite-index collision counter** tracks Dhan's recycling of
   IDs. If it trends > 5, Dhan released new reused IDs — update
   Grafana threshold or fix downstream lookups.

## Related files

- `.claude/rules/dhan/instrument-master.md` — Dhan CSV schema
- `.claude/rules/project/security-id-uniqueness.md` — composite-index invariant
- `.claude/rules/project/gap-enforcement.md` — I-P0 + I-P1 rules
- `crates/core/src/instrument/csv_downloader.rs` — download + cache
- `crates/core/src/instrument/universe_builder.rs` — filter + registry
- `crates/common/src/instrument_registry.rs` — composite index
- `scripts/auto-fix-refresh-instruments.sh` — trigger rebuild
- `deploy/docker/grafana/dashboards/data-lifecycle.json` — panels 1-4
