# Dhan 200-Depth Reproducer

Proof that Dhan's 200-level depth WebSocket works fine for our account
via their official Python SDK. Rust-side is broken; this isolates where.

## Run

```bash
cd scripts/dhan-200-depth-repro
python3.12 -m venv venv
source venv/bin/activate
pip install dhanhq==2.2.0rc1   # requires Python 3.10+ (SDK uses `match`)

export DHAN_CLIENT_ID='1106656882'
export DHAN_ACCESS_TOKEN='<fresh JWT from web.dhan.co Profile>'

python repro.py
```

Expected output: continuous 200-level bid/ask streaming, one block of
~200 rows every tick. Runs forever until Ctrl+C or a Dhan-side
disconnect.

First successful run: 2026-04-23, SecurityId 72271, 30+ min, zero disconnects.

## Why this exists

See `.claude/plans/active-plan.md`. Our Rust WebSocket client gets
`Protocol(ResetWithoutClosingHandshake)` within seconds of subscribing
to the same SecurityId. This Python script proves the cause is in our
Rust code, not in Dhan's server / our account / our token / our IP.

Next diagnostic: `cargo run --release --example depth_200_variants`
from `crates/core/` — runs 3 Rust variants in parallel against the
same SecurityId to isolate URL path vs header fingerprint vs TLS lib.

## Security

- Token is read from `DHAN_ACCESS_TOKEN` env var, never hardcoded.
- `venv/` is `.gitignore`'d and must not be committed.
- Token should be revoked from web.dhan.co after the test run.
