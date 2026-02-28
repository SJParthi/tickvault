# Secrets & Logging

## Secrets
- SSM naming: `/dlt/<env>/<service>/<key>`
- `Secret<String>` wraps all secret values
- Dhan JWT: 24h cycle, refresh at 23h, arc-swap atomic swap
- Zeroize on drop — always
- Failures = halt + alert, never fail silent
- Full reference: `docs/reference/secret_rotation.md`

## Logging
- tracing macros ONLY (no println!, no dbg!, no eprintln!)
- NEVER log secrets — secrecy crate enforces `[REDACTED]`
- Every log includes: What / Where / When / Which / Why
- ERROR level triggers Telegram alert
- Full reference: `docs/reference/logging_standards.md`

## Data Integrity
- Every write idempotent
- Orders use idempotency keys in Valkey BEFORE submission
- Ticks deduplicate by (security_id, exchange_timestamp, sequence_number)
- Position reconciliation after every fill — mismatch = halt trading
- Data retained 5 years (SEBI requirement)
- SEV-1/SEV-2 incidents: halt trading first, diagnose second
