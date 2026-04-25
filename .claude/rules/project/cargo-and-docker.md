---
paths:
  - "**/Cargo.toml"
  - "**/Dockerfile*"
  - "**/docker-compose*.yml"
  - "deploy/**"
---

# Cargo & Docker Rules

## Cargo
- Exact versions ONLY from Bible. `^`, `~`, `*`, `>=` are BANNED.
- `cargo update` is BANNED — Bible updates only.
- Workspace deps in root Cargo.toml, crates use `{ workspace = true }`
- Adding deps: check Bible first. Not found? Propose to Parthiban with justification.
- `edition = "2024"`, `rust-version = "1.95.0"` in every crate

## Docker
- Same docker-compose.yml and Dockerfile on Mac and AWS
- All infra in containers (host: Docker + Git + gh CLI only)
- Service discovery via Docker DNS — never localhost in app code
- Parity: fresh clone + `docker compose up -d` = identical behavior
- No `:latest` tags — use exact versions with SHA256
