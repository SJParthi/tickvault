//! Daily data-integrity cross-verification — Z+ L3 RECONCILE.
//!
//! Per `docs/architecture/aws-indices-only-locked-architecture.md` §14
//! and `.claude/plans/aws-lifecycle/THE-FINAL-PLAN.md` §5 (PR #9).
//!
//! Two daily gates compare our derived live candles against Dhan REST
//! as the authoritative ground truth:
//!
//! - **15:31 IST intraday gate** — 4 locked IDX_I SIDs × 3 timeframes
//!   (1m, 5m, 15m) = 12 verification pairs. Each derived `candles_<tf>`
//!   series is compared against Dhan REST `/v2/charts/intraday` for the
//!   same trading-date timestamps. Zero-tolerance OHLCV match.
//! - **08:05 IST morning gate** — yesterday's derived 1d candle vs
//!   Dhan REST `/v2/charts/historical`.
//!
//! Without this layer, missed ticks are invisible: a single dropped
//! WebSocket packet silently corrupts the OHLCV the strategy trades on.
//!
//! # Scope
//!
//! This module ships the pure-logic [`comparator`] — the zero-tolerance
//! OHLCV comparison engine and its verdict types. The schedulers, the
//! Dhan REST integration and the boot wiring land in subsequent
//! sub-PRs (#9b onward). The comparator is fully unit-tested in
//! isolation here.
//!
//! # Table-free (QuestDB table-cleanup #T5, 2026-05-20)
//!
//! Cross-verify is **table-free**. The comparison sources are Dhan
//! REST and the `historical_candles` table (read-only); the verdict is
//! reported **via Telegram only** plus the structured `error!` log
//! (`data/logs/errors.jsonl.*`). There is NO `cross_verify_audit` /
//! `cross_verify_mismatches` QuestDB table — the #9b+ schedulers MUST
//! NOT add an audit-table writer. See
//! `.claude/rules/project/option-chain-cross-verify-error-codes.md` §2.

pub mod comparator;
