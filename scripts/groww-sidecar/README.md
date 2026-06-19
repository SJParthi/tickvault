# Groww validation sidecar (Python, LOCAL-ONLY)

> **Operator lock §32** (`groww-second-feed-scope-2026-06-19.md`): the Groww feed
> uses the official **Python `growwapi` SDK** for validation, run as a **separate
> local process on the operator's Mac** (Dhan OFF / Groww ON). It is **default-OFF
> + isolated**, writes ONLY to a local NDJSON file, and **never** runs in AWS/prod
> without a further dated operator quote.
>
> **HONEST STATUS:** this sidecar was written *correct-by-construction against the
> Groww docs* — it could **not** be run/verified in the build sandbox (no
> `growwapi`, no network to Groww). Every line that depends on the exact SDK shape
> is marked `# VERIFY`. **Run `groww_smoke.py` FIRST** to see the real tick object,
> then we lock the field mapping in `groww_sidecar.py`.

## What it does
The sidecar is the **producer**. The Rust **bridge** (`crates/app/src/groww_bridge.rs`,
already merged) is the **consumer**: it tails the file this sidecar writes and
persists `groww_live_ticks` + `groww_candles_1m`.

```
growwapi GrowwFeed ──(callback)──► append NDJSON line ──► data/groww/live-ticks.ndjson
                                    (capture-at-receipt,                     │
                                     fsync per line)                         ▼
                                                          Rust bridge tails it → QuestDB
```

## The NDJSON contract (the bridge parses EXACTLY this)
One JSON object per line:
```json
{"security_id":1333,"segment":"NSE_EQ","ts_ist_nanos":1780000020123000000,"exchange_ts_millis":1780000020123,"ltp":2847.55,"cum_volume":123456}
```
| field | type | meaning |
|---|---|---|
| `security_id` | int | the Dhan-equivalent id for this instrument (Groww `exchange_token`, or a mapped id) |
| `segment` | str | one of `IDX_I` / `NSE_EQ` / `NSE_FNO` / `BSE_EQ` / `BSE_FNO` / … |
| `ts_ist_nanos` | int | tick time as **IST epoch nanoseconds** (sidecar converts) |
| `exchange_ts_millis` | int | the raw Groww millisecond timestamp, verbatim |
| `ltp` | float | last traded price |
| `cum_volume` | int | cumulative day volume |

## Run it (on your Mac)
```bash
cd scripts/groww-sidecar
python3 -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt

export GROWW_API_KEY="...your TOTP token..."
export GROWW_TOTP_SECRET="...your TOTP secret..."

# STEP 1 — de-risk: see the REAL tick object shape (prints 5 ticks, then exits)
python3 groww_smoke.py
#   → paste the printed objects back to me so I lock the field mapping.

# STEP 2 — run the full sidecar (writes the NDJSON the Rust bridge reads)
python3 groww_sidecar.py
#   → check data/groww/live-ticks.ndjson is filling with valid lines.

# STEP 3 — start tickvault with Groww ON (Dhan can stay on too); the bridge
#   tails the file → groww_live_ticks + groww_candles_1m in QuestDB.
```

## Honest envelope
- Zero-tick-loss is preserved as **capture-at-receipt**: each tick is `write()` +
  `flush()` + `os.fsync()` to the append-only file the instant the callback fires,
  *before* the Rust side sees it. A Python crash in the microsecond between
  socket-recv and fsync is a tiny bounded loss window — far better than none, and
  honestly weaker than Dhan's at-socket capture (documented in lock §32.3).
- O(1) is **not** claimed on the Python hop (GIL/GC). It **is** preserved on the
  Rust consumer (the bridge + aggregator).
- The exact Groww→NDJSON field mapping is **VERIFY-gated** — confirmed by STEP 1.
