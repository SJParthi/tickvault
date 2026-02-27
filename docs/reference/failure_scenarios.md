# Failure Scenarios — End-to-End Test Checklist

> Extracted from CLAUDE.md Gate 3. Claude Code must write tests for each scenario as it becomes relevant to the current phase.

## Network Failures
- WebSocket disconnects mid-tick → Must reconnect + resume
- Dhan API returns 429 (rate limited) → Must back off per governor
- Dhan API returns 500/502/503 → Must retry per backon
- DNS resolution fails → Must use circuit breaker (failsafe)
- TLS handshake timeout → Must retry, not crash
- Internet goes down completely → Must alert via SMS (SNS), pause trading

## Data Failures
- Malformed binary packet from Dhan → Must reject + log, not crash
- Zero-length WebSocket frame → Must handle gracefully
- Duplicate tick data → Must deduplicate, not double-count
- Out-of-order timestamps → Must detect + handle
- QuestDB write fails → Must buffer + retry, not lose data
- Valkey connection drops → Must reconnect via deadpool, serve stale if needed

## State Failures
- Application crash mid-processing → Must recover state from memmap2
- OMS in invalid state → Must reject order + alert, not proceed
- Config file corrupted/missing → Must fail fast with clear error, not use defaults
- Secrets not available in SSM → Must fail fast, not start with empty credentials
- Market holiday not in calendar → Must refuse to trade, not submit orders

## Infrastructure Failures
- Docker container OOM killed → Must restart via Docker restart policy
- EBS volume full → Must alert before 90%, stop writes at 95%
- EC2 instance hardware failure → Must auto-recover via CloudWatch
- Process hangs (no heartbeat) → Must auto-restart via sd-notify watchdog

## Trading Safeguards
- Position size exceeds risk limit → Must reject order + alert
- P&L drawdown exceeds daily limit → Must halt all trading + alert
- Order rejected by exchange → Must update OMS state + log reason
- Partial fill received → Must track correctly, not treat as full fill
- Market closes while order pending → Must cancel open orders + reconcile
