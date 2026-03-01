# Logging Standards

> Rules summary: `.claude/rules/rust-code.md` (auto-loaded).
> This doc is for deep implementation detail only — JSON format, log levels, retention.

## Structured JSON Format (tracing-subscriber)
```json
{
  "timestamp": "2026-03-15T10:30:45.123456789+05:30",
  "level": "INFO",
  "target": "dhan_live_trader::websocket_client",
  "span": "process_tick",
  "message": "Tick processed successfully",
  "fields": { "security_id": 12345, "exchange": "NSE", "latency_ns": 8500 }
}
```

## Log Levels
| Level | Use For | Examples |
|-------|---------|----------|
| ERROR | System failures → triggers Telegram alert | WebSocket disconnect, QuestDB write failure, secret rotation failure |
| WARN  | Degraded conditions | Tick latency above threshold, retry attempts, duplicate tick |
| INFO  | Operational events (sparse) | Startup/shutdown, config loaded, token refreshed, market open/close |
| DEBUG | Diagnostic (disabled in prod) | Tick processing steps, cache hit/miss, channel utilization |
| TRACE | Extremely verbose (never in prod) | Raw packet bytes, full struct dumps |

## NEVER Log (Security Incidents)
- API keys, tokens, passwords, TOTP secrets — use `Secret<T>` from secrecy crate
- Raw JWT values (log expiry time only)
- SSM Parameter Store values
- Raw binary WebSocket frames (log parsed summary only)
- Any raw secret in logs = SECURITY INCIDENT → fix + rotate credential

## Log Context
See `.claude/rules/rust-code.md` for the 5W rule (What/Where/When/Which/Why). Enforcement is auto-loaded per path.

## Retention
- Hot (Loki): 30 days | Cold (S3): 5 years (SEBI) | Rotation: Alloy + Loki policies
