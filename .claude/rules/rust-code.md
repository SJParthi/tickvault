---
paths:
  - "crates/**/*.rs"
---

# Rust Code Rules

## Error Handling
- No `.unwrap()` or `.expect()` in prod — use `?` with anyhow/thiserror
- No `#[allow()]` without `// APPROVED:` comment
- Fix root cause, never suppress

## Naming
- Types: PascalCase | Functions: snake_case | Constants: SCREAMING_SNAKE
- Crates: kebab-case | Enum variants: PascalCase
- No abbreviations: `fn parse_ticker_packet(raw_bytes: &[u8])` not `fn parse(b: &[u8])`

## Logging
- tracing macros ONLY (no println!, dbg!, eprintln!)
- NEVER log secrets — `Secret<T>` from secrecy crate enforces `[REDACTED]`
- Every log: What / Where (#[instrument]) / When (IST) / Which (security_id) / Why (error chain)
- ERROR level triggers Telegram alert

## No Hardcoded Values
- Raw numbers or string literals in app code = bug
- Use named constants or config values

## Secrets & Infrastructure
- All secrets from AWS SSM Parameter Store (`Secret<String>`, zeroize on drop)
- No localhost — use Docker DNS hostnames
- Failures = halt + alert, never fail silent

## Deep Reference
- Logging: `docs/reference/logging_standards.md`
- Secrets: `docs/reference/secret_rotation.md`
