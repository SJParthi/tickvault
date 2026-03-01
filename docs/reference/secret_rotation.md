# Secret Rotation & Token Lifecycle — Full Reference

> Rules summary: `.claude/rules/rust-code.md` (auto-loaded).
> This doc is for deep implementation detail only — token lifecycle, rotation rules, SSM naming.

## Dhan Token Lifecycle

Dhan uses a 24-hour JWT token cycle. If rotation fails, the system goes blind (no market data) and deaf (no order placement).

```
1. Session init     → reqwest POST to Dhan REST API with credentials
2. JWT issued       → Valid for 24 hours from issue time
3. Token stored     → arc-swap atomic pointer swap (O(1), no lock)
4. Token used       → Every WebSocket frame + REST call reads via arc-swap
5. Token refresh    → Must happen BEFORE expiry, not after
6. Token wiped      → Old token zeroized via zeroize crate on drop
```

## Rotation Rules

```
REFRESH WINDOW:    Refresh token at 23 hours (1 hour before expiry)
RETRY ON FAILURE:  Exponential backoff 100ms → 30s (backon crate)
MAX RETRIES:       5 attempts before circuit breaker trips (failsafe)
ALERT ON FAILURE:  Telegram alert after 2nd consecutive failure
HALT ON FAILURE:   If all 5 retries fail → halt trading, alert via SNS
NEVER:             Never cache token to disk. Never log token value.
```

## SSM Parameter Store Secret Names

```
Secret naming convention:  /dlt/<environment>/<service>/<key>
Examples:
  /dlt/prod/dhan/client-id
  /dlt/prod/dhan/client-secret
  /dlt/prod/dhan/totp-secret
  /dlt/dev/dhan/client-id
```

## Security Requirements

- All secret values wrapped in `Secret<String>` (secrecy crate) — Debug prints `[REDACTED]`
- All secret memory wiped on drop via `zeroize` — write_volatile + memory fences
- NEVER log, print, serialize, or transmit raw secret values
- NEVER store secrets in Valkey, QuestDB, or any persistent store
- Secret access failures must fail LOUD (alert + halt), never fail silent
