# Backtest Runner — Mac-Side Strategy Param Search Workflow

> **Authority:** Parthiban (architect). Claude Code (executor).
> **Scope:** Pre-Phase-1 strategy parameter discovery + walk-forward
> validation. Runs on dev Mac, NEVER on AWS prod.
> **Output:** Winning param set committed to `config/strategy/<name>.toml`
> + walk-forward verdict committed to `.claude/plans/strategy-verdicts/`.

## Why backtests are Mac-only

  * AWS prod is a TRADING instance, not a research instance.
  * Backtest sweeps are CPU-bound batch work (Criterion-style); they
    starve the live tick pipeline if co-located.
  * Historical data downloads at high QPS would trigger Dhan DH-904
    rate-limiter against the prod token. Mac dev uses a separate
    research token.

## The 4-stage workflow

```
Stage 1 — Fetch historical
   └─ Dhan REST /v2/charts/historical (90-day chunks)
   └─ Persist to local QuestDB (NOT prod)
   └─ Stage gate: row count + cross-verify
        ↓
Stage 2 — Sweep params
   └─ Generate param grid from strategy schema
   └─ Run each combination through the strategy engine (offline mode)
   └─ Persist signal_audit + decision_audit + synthetic_pnl_audit
   └─ Stage gate: every combination produced ≥ 1 audit row
        ↓
Stage 3 — Walk-forward validate
   └─ Split historical into N folds (e.g., 6 × 30-day folds)
   └─ Train (param-pick) on fold[k], test on fold[k+1]
   └─ Stage gate: top-decile params consistent across ≥ 4/5 folds
        ↓
Stage 4 — Commit winning params
   └─ Write `config/strategy/<strategy_name>.toml` with winners
   └─ Write `.claude/plans/strategy-verdicts/<date>-<name>.md`
   └─ Open PR citing the verdict file
   └─ Stage gate: PR review + adversarial 3-agent grill
```

## Stage 1 — Fetch historical

### Pre-conditions

  * Dev Mac has `tickvault` cloned + Docker daemon running
  * `make docker-up` brings up local QuestDB (Valkey/Grafana removed in #O4, 2026-05-24)
  * Research Dhan token in `~/.aws/credentials` SSM path
    `/tickvault/dev-research/dhan/access-token`
  * Free disk ≥ 100 GB on Mac (90 days × 222 SIDs × 5 timeframes ≈
    ~40 GB compressed)

### Run

```bash
# From repo root
cargo run --release --bin tv-backtest-fetch -- \
    --strategy <name> \
    --from-date 2025-01-01 \
    --to-date   2026-04-30 \
    --timeframes 1m,5m,15m,60m,1d
```

### Stage gate

  * `SELECT count(*) FROM historical_candles WHERE trading_date_ist
    BETWEEN '<from>' AND '<to>'` returns the expected row count
    (90 trading days × 222 SIDs × 5 timeframes per chunk).
  * Cross-verify: pick 3 random `(security_id, timeframe, ts)` tuples
    and compare against Dhan REST again. Must match exactly.
  * Gap-free: no `(security_id, timeframe)` pair has > 1 trading day
    of missing candles.

### Failure recovery

  * **DH-904 rate-limit during fetch:** the per-timeframe retry
    ladder (`compute_dh904_backoff` in `crates/trading/src/oms/`)
    handles it. Total fetch time may extend by 30-60 minutes on a
    bad day.
  * **DH-805 too many connections:** wait 60s + audit any other
    process holding research token connections. Restart fetch.
  * **Missing day in middle of range:** Dhan sometimes drops a day
    silently. Re-run fetch for that specific date.

## Stage 2 — Sweep params

### Pre-conditions

  * Stage 1 complete + green stage gate
  * Strategy implementation deployed to dev Mac in `crates/trading/src/strategy/<name>/`
  * Param schema defined in `crates/trading/src/strategy/<name>/params.rs`
    with `#[derive(BacktestSweep)]` macro (placeholder; macro lands
    when first real strategy ships)

### Run

```bash
cargo run --release --bin tv-backtest-sweep -- \
    --strategy <name> \
    --grid-config config/backtest/<name>-grid.toml \
    --jobs 8                                       # parallelism
```

### Stage gate

  * Every param combination produced ≥ 1 signal_audit row.
  * Every combination produced ≥ 1 decision_audit row.
  * Synthetic P&L tracked per-combination in `synthetic_pnl_audit`.
  * No NaN/Inf in any indicator snapshot (Item 21 guard active).

### Failure recovery

  * **OOM on a parallel job:** lower `--jobs`. Each job loads the
    indicator engine state in RAM (~500 MB per job).
  * **NaN in indicator output:** the sanitize_nan_inf guard clamps
    + counts; investigate which input row caused NaN (price = 0?
    volume divide-by-zero?).

## Stage 3 — Walk-forward validate

### Pre-conditions

  * Stage 2 complete + green stage gate
  * At least 6 × 30-day folds available (180 trading days of data)

### Run

```bash
cargo run --release --bin tv-backtest-walk-forward -- \
    --strategy <name> \
    --folds 6 \
    --train-fold-size 30 \
    --test-fold-size  30
```

### Output

```
fold 1: train 2025-01..2025-01, test 2025-02..2025-02
        top 3 params by sharpe: [...]
        top 3 by win-rate:      [...]
fold 2: ...
...
Overall: param X consistent in top-decile across 5/6 folds → WINNER
         param Y consistent in 4/6 folds            → CANDIDATE
         param Z inconsistent (top in fold 1, bottom in fold 5) → REJECT
```

### Stage gate

  * At least 1 param set in top-decile across ≥ 4/5 consecutive folds.
  * Winning set NOT solely determined by 1 outlier fold (e.g., a
    single high-volatility month).
  * Sharpe of winning set ≥ baseline (TBD — first real strategy will
    establish the floor).

### Failure recovery

  * **No param set passes ≥ 4/5 consistency:** strategy is not robust
    enough. Either (a) tune the param schema bounds, (b) rebuild the
    strategy logic, OR (c) reject the strategy entirely.

## Stage 4 — Commit winning params

### Pre-conditions

  * Stage 3 complete + green stage gate
  * Walk-forward verdict file drafted

### Run

```bash
# 1. Write config
cat > config/strategy/<name>.toml <<EOF
[strategy.<name>]
enabled    = true
dry_run    = true                      # ALWAYS true until Phase 1
<param1>   = <winning_value_1>
<param2>   = <winning_value_2>
...
EOF

# 2. Write verdict file
cat > .claude/plans/strategy-verdicts/$(date +%Y-%m-%d)-<name>.md <<EOF
# Strategy <name> — Walk-Forward Verdict

**Window:** YYYY-MM-DD to YYYY-MM-DD
**Folds:** 6 × 30 days
**Top param:** {param1: V1, param2: V2, ...}
**Sharpe (winning fold-avg):** S
**Win rate (winning fold-avg):** W%
**Consistency:** top-decile in N/6 folds
**Recommendation:** PROMOTE TO PHASE 1 / REJECT / NEEDS MORE DATA
EOF

# 3. Open PR
git checkout -b strategy/<name>-promote-dry-run
git add config/strategy/<name>.toml .claude/plans/strategy-verdicts/
git commit -F /tmp/commit-msg-strategy.txt
git push -u origin strategy/<name>-promote-dry-run
```

### Stage gate

  * PR review by Parthiban
  * Adversarial 3-agent grill on the diff (per operator charter §E)
  * CI green

### Phase 1 promotion path

This Stage 4 PR adds the strategy to dry-run only. **`dry_run = true`
remains the default for 22 trading days** while
`docs/runbooks/phase-1-monitoring-rubric.md` is run end-to-end.

Only AFTER the rubric verdict is PASS does a second PR flip
`dry_run = false` for that strategy.

## Anti-patterns (FORBIDDEN)

  * **Running backtests against prod QuestDB.** Always use a
    separate local docker-compose with its own data volume.
  * **Using the prod Dhan token for historical fetch.** Use the
    research SSM path.
  * **Cherry-picking a single profitable fold and declaring victory.**
    Walk-forward consistency is the only valid criterion.
  * **Committing winning params without a verdict file.** The verdict
    file is the audit trail; without it, the param values have no
    justification.
  * **Promoting `dry_run = false` in the same PR as the strategy
    config.** Two separate PRs ALWAYS — one for the strategy, one for
    the live flip after rubric PASS.

## Cross-references

  * Strategy implementation paths: `crates/trading/src/strategy/`
  * Phase 1 monitoring rubric: `docs/runbooks/phase-1-monitoring-rubric.md`
  * Operator daily startup: `docs/runbooks/operator-daily-startup.md`
  * Phase 0 architecture: `.claude/rules/project/phase-0-architecture.md`
  * Honest 100% claim wording: `.claude/rules/project/operator-charter-forever.md` §F

## Honest 100% claim (for backtest results)

> "100% inside the tested envelope, with ratcheted regression coverage:
> 6×30-day walk-forward consistency, ≥ 99.5% historical cross-verify
> match, zero NaN/Inf guard fires during sweep, baseline Sharpe floor
> enforced. Beyond the envelope, past performance does NOT predict
> future returns; this workflow proves the STRATEGY IS ROBUST across
> the sampled regime, NOT that it will make money tomorrow."
