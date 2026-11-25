# brutex strategy contract — what tickvault needs from brutex

> **Owner decisions, 29 Oct 2026 (final).** Recorded in
> `.claude/rules/project/brutex-strategy-host-2026-10-29.md`. This file is the
> technical contract both repositories build against. tickvault never writes to
> brutex or to the recipe bucket; brutex never changes tickvault.

## 1. Who does what

| Step | brutex | tickvault |
|---|---|---|
| Find and backtest strategies | ✅ only here | ❌ never |
| Approve for live | the owner | — |
| Strategy logic (candles, indicators, patterns, per-stock numbers, entry, exit, stop-loss, target) | ✅ one standalone Rust crate | ❌ none of its own |
| Publish approved versions and snapshots | ✅ `s3://tv-prod-cold/strategy-recipes/` | read only |
| Prove the live math matches the backtest | provides check files | replays them, bar by bar |
| Run it live, place orders | — | ✅ paper first, then real money |

The existing `crates/trading/src/indicator` and `crates/trading/src/strategy`
modules hold none of the owner's strategies and are never used to trade a
brutex strategy.

## 2. The strategy crate (brutex provides)

One crate, e.g. `brutex-strategy`, in the brutex repository. tickvault depends
on it as:

```toml
brutex-strategy = { git = "https://github.com/SJParthi/brutex", rev = "<full 40-char sha>" }
```

The `rev` is the exact commit that produced the approved backtest. A new
approved version moves the pin in a reviewed tickvault release; nothing moves
it automatically.

### 2.1 Hard requirements on the crate

| # | Requirement | Why |
|---|---|---|
| R1 | Pure Rust, edition 2024, builds on stable 1.95.0; no `build.rs` that runs another language, no C/C++ dependency, no proc-macro that reads files or the network | tickvault's Rust-only lock and cargo-deny |
| R2 | Every dependency pinned exactly (`=x.y.z`) and drawn from crates.io only; no further git dependencies | tickvault's version-pinning rule |
| R3 | Deterministic: the same inputs give bit-identical outputs on x86-64 and aarch64 (Graviton). No `f64::mul_add`/FMA, no platform `libm` transcendental functions (use a pure-Rust libm), no `HashMap` iteration order in results, no time, no randomness | the replay match runs on the Graviton server against check files made on the backtest machine |
| R4 | No I/O, no logging requirement, no threads, no locks, no blocking, no `async` | it runs inside tickvault's hot path |
| R5 | No allocation per tick or per bar after `new()`; all state sized at construction from the settings and instrument count | tickvault's zero-allocation rule (DHAT-tested) |
| R6 | O(1) per tick and per bar in the number of instruments; p99 under 1 µs per tick on one core | the owner's latency target |
| R7 | No panics on any input (NaN, zero, negative, out-of-order or duplicate timestamps, a bar with volume 0); errors are returned values | tickvault denies `unwrap`/`expect` and cannot crash mid-session |
| R8 | Instruments are always `(security_id: u64, exchange_segment: u8)`, never the id alone | tickvault's I-P1-11 uniqueness rule |
| R9 | Each approved commit is tagged `strategy/<name>/v<N>` and is never deleted, rebased or force-pushed | a pinned `rev` that disappears breaks the build (happened 2026-08-07) |
| R10 | `cargo test` in brutex replays the published check files through the crate and matches them exactly | the same proof tickvault runs |

### 2.2 The API tickvault codes against

tickvault's side is the trait `StrategyEngine` in
`crates/trading/src/brutex_host/engine.rs`. An adapter maps the brutex crate's
API onto it, so the shape below is what tickvault needs, not a demand on names:

```rust
pub trait StrategyEngine {
    /// Called once, before the open. Sizes all state. May allocate.
    fn configure(&mut self, recipe: &Recipe, params: &ParamSnapshot,
                 universe: &[InstrumentKey]) -> Result<(), EngineError>;
    /// One live tick. Hot path: no allocation, no I/O, O(1).
    fn on_tick(&mut self, tick: &TickInput, out: &mut IntentSink);
    /// One closed candle (replay and candle-driven strategies). Same rules.
    fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink);
    /// Indicator values for the last bar of an instrument, for the replay check.
    fn bar_values(&self, key: InstrumentKey, out: &mut ValueRow);
}
```

**Inputs tickvault can supply live:**

| Field | Type | Notes |
|---|---|---|
| `security_id` | `u64` | with `exchange_segment` always |
| `exchange_segment` | `u8` | Dhan segment code (0 IDX_I, 1 NSE_EQ, 2 NSE_FNO, …) |
| `exchange_time_ist_secs` | `u32` | the exchange's last-trade time, IST seconds (whole seconds is what Dhan sends) |
| `receipt_ns` | `i64` | when tickvault received it |
| `ltp` | `f64` | from Dhan's f32, cleaned to 2 decimals the way tickvault stores it |
| `cumulative_volume` | `u64` | day volume |
| `ltq`, `atp`, `oi`, day `open/high/low/close` | optional | present on Quote/Full packets |
| closed candles | `BarInput` | tickvault's own fold, timeframes 1s 1m 3m 5m 10m 15m 30m 60m, sessions from 09:15 IST |

**Outputs tickvault needs:** `OrderIntent` written into a fixed-capacity
`IntentSink` (no allocation): instrument, side, quantity in lots, entry type
(market/limit + price), **stop-loss price**, **target price**, optional trailing
step, a strategy-assigned intent id (u64, unique per day) and the bar/tick time
it fired on. tickvault applies its own risk checks and order rules after this.

**Open question for the owner:** live input = tickvault's own candles, or raw
ticks into the crate's own candle builder? The trait carries both; the replay
check uses bars. Recommended: bars from tickvault's fold (already
cross-checked against Dhan's minute tape daily) for candle strategies, ticks
only for strategies that need them.

## 3. The published files (brutex provides)

```
s3://tv-prod-cold/strategy-recipes/<name>/
  v<N>/                         the approved METHOD; never overwritten
    recipe.json
    check_candles.csv           the candles used in the backtest run
    check_values.csv            every indicator value, every bar
    check_signals.csv           every buy/sell/exit signal, every bar
    manifest.json               sha256 + byte size of every file in this folder
    params/<YYYY-MM-DD>/        PARAMETER snapshot (dynamic strategies)
      params.json, check_candles.csv, check_values.csv, check_signals.csv,
      gates.json, manifest.json
  universe/<YYYY-MM-DD>/        UNIVERSE snapshot: that day's F&O list
    universe.json, manifest.json
```

Files may be `.csv` or `.csv.gz`; the manifest names the exact file.

### 3.1 `recipe.json` (schema `tickvault-strategy-recipe/1`)

```json
{
  "schema": "tickvault-strategy-recipe/1",
  "name": "orb_fno_v",
  "version": 3,
  "status": "approved",
  "approved_by": "Parthiban",
  "approved_at_ist": "2026-11-02T18:05:00+05:30",
  "brutex_commit": "<40-char sha>",
  "crate": "brutex-strategy",
  "crate_version": "0.4.0",
  "dynamic": true,
  "instruments": [ { "security_id": 2885, "exchange_segment": 1 } ],
  "timeframes": ["1m", "5m"],
  "settings": { "...": "strategy-specific, passed to the crate unchanged" },
  "entry": "...", "exit": "...", "stop_loss": "...", "target": "...",
  "backtest": { "from": "2018-09-03", "to": "2026-10-28", "trades": 1234,
                "win_rate": 0.51, "net_pnl": 0.0, "max_drawdown": 0.0,
                "data_source": "zerodha-1m+gdfl-ticks" },
  "sanity_limits": { "max_param_change_fraction": 0.5 }
}
```

`instruments` is empty for a dynamic strategy (the universe snapshot supplies
it). `entry/exit/stop_loss/target` are human-readable descriptions; the crate
holds the logic.

### 3.2 `manifest.json`

```json
{ "schema": "tickvault-strategy-manifest/1",
  "files": [ { "name": "recipe.json", "sha256": "<64 hex>", "bytes": 1834 } ] }
```

Every file in the folder except `manifest.json` itself is listed; a listed file
that is missing, an unlisted extra file, a wrong size or a wrong hash refuses
the whole folder.

### 3.3 Check files (CSV, header row, comma, no quotes, `.` decimal)

- `check_candles.csv`: `ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume`
- `check_values.csv`: `ts_ist,security_id,exchange_segment,timeframe,<value columns…>` (one column per indicator value the crate exposes, names fixed by the recipe version)
- `check_signals.csv`: `ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target`
  (`signal` ∈ `none,entry,exit`)

`ts_ist` is `YYYY-MM-DDTHH:MM:SS` in IST (bar open time). Floats are compared
bit for bit after parsing the text with Rust's `str::parse::<f64>`, so brutex
must print them with Rust's shortest round-trip format (`{}` on f64).

### 3.4 Snapshots

- `params/<date>/gates.json`: the Winner Gate result on the newest walk-forward
  window (`passed`, window dates, metric values) and the snapshot's numbers.
- `universe/<date>/universe.json`: `{ "date": "...", "instruments": [ {security_id, exchange_segment} ] }`.

A snapshot dated after the trading day is ignored; the newest snapshot dated on
or before the trading day that passes every gate is used.

## 4. What tickvault checks before using anything

1. Pick the newest `v<N>` with `status: approved`; a gap (v2 missing between v1
   and v3) is allowed but logged; a version whose recipe `version` field differs
   from its folder name is refused.
2. Verify the manifest (hashes, sizes, no missing, no extra files).
3. The recipe's `brutex_commit` must equal the commit tickvault is built
   against; otherwise refused (a new version needs a tickvault release).
4. Snapshot gates: `gates.json.passed`, sanity limits against the previous
   snapshot, universe not empty and every instrument known to tickvault.
5. Replay: feed `check_candles` through the pinned crate; compare every value
   and every signal bar by bar; the first difference (file, row, bar time,
   instrument, field, expected, got) refuses it.
6. Switch only before the open (before 09:00 IST); never mid-session. On any
   refusal the previous accepted version and snapshot stay live and the owner
   is alerted once.
7. Every load, refusal and swap is written to the audit log.

## 5. Read access to brutex on every build machine (decision 2)

tickvault can trade a strategy only if every machine that builds it can read
`github.com/SJParthi/brutex`. Read-only, never committed:

| Machine | How it reads brutex | Status (29 Oct 2026) |
|---|---|---|
| Claude cloud build machine | the session's git proxy | ✅ Verified: `git ls-remote https://github.com/SJParthi/brutex` listed `HEAD 4bbd37df…` and the branches, exit 0 |
| Owner's Mac | a fine-grained GitHub token, **read-only on SJParthi/brutex only** (Contents: Read), in the macOS keychain (`git credential-osxkeychain`) | ⏳ owner sets up |
| GitHub Actions CI | a deploy key on SJParthi/brutex with **write access unchecked**; its private half stored as the Actions secret `BRUTEX_READ_DEPLOY_KEY`; CI loads it into `ssh-agent` and sets `CARGO_NET_GIT_FETCH_WITH_CLI=true` with `url."ssh://git@github.com/SJParthi/brutex".insteadOf=https://github.com/SJParthi/brutex` | ⏳ owner adds the key and secret; the workflow step lands with the first dependency PR |
| AWS (the box only runs the binary; it builds nothing) | nothing needed today; if a build ever runs there, the same read-only key from SSM `/tickvault/<env>/brutex/read-deploy-key` | not needed |

The check every machine runs (harmless, read-only):

```
git ls-remote https://github.com/SJParthi/brutex HEAD
```

A machine that cannot read brutex cannot build tickvault with a strategy, so
it cannot trade one. That is intended: no access means no trading, never a
silent fallback.
