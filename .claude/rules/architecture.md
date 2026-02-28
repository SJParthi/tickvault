# Architecture Principles

## Docker is the Runtime
- Same docker-compose.yml and Dockerfile on Mac and AWS
- All infrastructure in containers (host only has Docker + Git + gh CLI)
- Service discovery via Docker DNS — never localhost in app code
- Parity test: fresh clone + `docker compose up -d` = identical behavior

## No Secrets on Disk
- All secrets from AWS SSM Parameter Store (SecureString)
- LocalStack for dev, real AWS for prod — same SDK, same code path
- Non-secret config from TOML files in git
- Only env difference: `AWS_ENDPOINT_URL`
- `Secret<String>` wraps all secrets. Zeroize on drop.

## Zero Hardcoded Values
```
WRONG: Duration::from_secs(10)         RIGHT: Duration::from_secs(PING_INTERVAL_SECS)
WRONG: "wss://api-feed.dhan.co/v2"     RIGHT: config.dhan.websocket_url
WRONG: if hour >= 9 && minute >= 15     RIGHT: current_time >= config.trading.market_open_time
```
Raw number or string literal in app code = bug.

## Naming Convention
- Types/Structs: PascalCase | Functions: snake_case
- Constants: SCREAMING_SNAKE | Modules: snake_case
- Crate names: kebab-case | Enum variants: PascalCase
- No abbreviations. `fn parse_ticker_packet(raw_bytes: &[u8])` not `fn parse(b: &[u8])`
