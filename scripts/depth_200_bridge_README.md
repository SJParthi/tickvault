# Depth-200 Python Sidecar Bridge

Production fallback for 200-level depth streaming when the Rust client
hits Dhan-side resets (chronic on expiry-day with ≥4 ATM contracts
subscribed concurrently from one `clientId`).

The Python SDK with the same wire signature streamed cleanly for 30+
minutes on 2026-04-23 against a single far-strike. This sidecar
replicates that exact wire pattern (root path `/?token=...`, raw
`websockets`, RequestCode 23 flat JSON) and writes parsed depth into
the same `deep_market_depth` QuestDB table the Rust client uses, so
existing dashboards and downstream consumers see no schema change.

## Files

| Path | What |
|---|---|
| `scripts/depth_200_bridge.py` | The sidecar — asyncio, one task per SID, ILP-over-TCP to QuestDB |
| `scripts/depth_200_bridge_requirements.txt` | Pinned `websockets>=12,<16` |
| `scripts/test_depth_200_bridge.py` | 19 unit tests for parser + ILP builder + Subscription parsing |

## Run it

```bash
# 1. Install deps (one-time):
pip install -r scripts/depth_200_bridge_requirements.txt

# 2. Set credentials (or rely on data/cache/tv-token-cache that the Rust app writes):
export DHAN_CLIENT_ID='1106656882'
export DHAN_ACCESS_TOKEN='eyJ...'   # fresh JWT from web.dhan.co or our token cache

# 3. Subscribe to the 4 ATM contracts your Rust app currently tracks:
python3 scripts/depth_200_bridge.py \
    --sid 72265:NSE_FNO \
    --sid 72266:NSE_FNO \
    --sid 67522:NSE_FNO \
    --sid 67523:NSE_FNO
```

Verify rows are landing:

```sql
-- in QuestDB console at http://localhost:9000
SELECT count(*) AS rows_last_1m FROM deep_market_depth
  WHERE depth_type = '200' AND ts > dateadd('m', -1, now());
```

You should see > 0 rows growing every second once subscriptions are live.

## How it stays alongside the Rust app

The Rust client and this Python sidecar both write to
`deep_market_depth`. The QuestDB DEDUP key (per
`crates/storage/src/deep_depth_persistence.rs`) absorbs any duplicates
across sources. While Rust keeps flapping it produces some rows;
Python produces the rest. Net coverage = both combined.

When v1 lands, the Rust 200-depth connection task will be gated behind
a config flag (`depth_200.use_python_bridge = true`) so the sidecar
becomes the sole writer.

## Tests

```bash
python3 -m unittest scripts.test_depth_200_bridge -v
```

19 tests cover frame parsing (BID/ASK/disconnect/zero-padding/segment
mapping), ILP line format (level indexing, microsecond suffix, symbol
ordering), Subscription argument parsing, and credential loading
precedence. Network paths are NOT covered here — they need an
integration harness with a live QuestDB and a mock Dhan WebSocket
server, which is a follow-up.

## Known gaps (v0 — what's missing)

1. **No automatic ATM rebalance.** Operator passes a fixed list of
   SIDs at start. When spot drifts and the Rust depth-rebalancer
   issues a `Swap200` command, the Python sidecar does NOT see it. v1
   plan: Rust writes the active SID list to a file
   (`data/depth-200-bridge/active_sids.json`); Python watches it via
   `inotify` / poll and re-subscribes.
2. **No Docker compose service.** Run manually for now. v1: add a
   `tv-depth-200-bridge` service to `deploy/docker/docker-compose.yml`
   with `restart: unless-stopped` so it survives crashes.
3. **No Rust gating.** Both Rust and Python write to QuestDB
   simultaneously. DEDUP handles duplicates so this is correctness-safe
   but is wasteful. v1 adds the config flag.
4. **No prometheus counter.** Operator can `tail -f` the stderr log to
   confirm streaming, but there's no metric showing
   `tv_depth_200_bridge_frames_total`. v1 adds it.
