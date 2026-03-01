# Security Policy — dhan-live-trader

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

### How to Report

1. **Email:** Contact the repository owner directly via GitHub
2. **GitHub Security Advisories:** Use the "Report a vulnerability" button in the Security tab

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

### Response Timeline

- **Acknowledgment:** Within 24 hours
- **Initial Assessment:** Within 72 hours
- **Fix/Patch:** As soon as possible, depending on severity

## Security Practices

This project follows strict security practices defined in `CLAUDE.md`:

- **No secrets on disk** — All credentials via AWS SSM Parameter Store
- **No `.env` files** — Banned. All secrets from AWS SSM Parameter Store
- **Secret types use `secrecy` crate** — `Debug` prints `[REDACTED]`
- **Memory wiped on drop** — `zeroize` crate for secret cleanup
- **Rate limiting** — SEBI-compliant 10 orders/second maximum
- **2FA mandatory** — TOTP for every API session
- **Audit trail** — Every order state transition logged
- **Dependencies audited** — `cargo audit` + `cargo deny` in CI
- **Supply chain tracked** — `cargo-vet` for dependency review

## Supported Versions

| Version | Supported |
|---------|-----------|
| Latest `main` | Yes |
| `develop` | Yes (pre-release) |
| Older tags | No |
