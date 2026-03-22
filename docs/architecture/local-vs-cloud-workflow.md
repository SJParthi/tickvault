# Local vs Cloud Workflow

> **Purpose:** Guide for which CI tasks run locally vs on cloud (GitHub Actions / Claude Code web).

## Quick Reference

| Task | Where | Why |
|------|-------|-----|
| `cargo check` | Cloud OK | Fast, ~1 min |
| `cargo test` | Cloud OK | ~3 min, moderate disk |
| `cargo clippy` | Cloud OK | ~2 min |
| `cargo fmt --check` | Cloud OK | Instant |
| `cargo build --release` | Cloud OK | ~5 min, target/ ~800MB |
| `cargo bench` | **Local only** | Needs stable hardware for consistent timings |
| `cargo llvm-cov` | **Local only** | Doubles build artifacts (~1.6GB) |
| `cargo mutants` | **Local only** | 50GB+ artifacts, hours of compute |
| `cargo fuzz` | **Local only** | Long-running, unbounded disk |
| ASan / TSan | **Local only** | Requires nightly, large artifacts |
| `cargo careful` | **Local only** | Requires nightly toolchain |

## Why Some Tasks Are Local Only

### Coverage (`cargo-llvm-cov`)
- Instruments every function → doubles `.o` file sizes
- Target directory grows from ~800MB to ~1.6GB
- Cloud environments may hit disk limits on free tiers

### Mutation Testing (`cargo-mutants`)
- Generates one modified binary per mutation
- For ~61K LoC, expect 50GB+ of artifacts
- Runtime: hours, not minutes
- Will absolutely exhaust cloud disk quotas

### Fuzzing (`cargo-fuzz`)
- Runs indefinitely by default
- Corpus grows without bound
- Needs local corpus management

### Sanitizers (ASan, TSan)
- Require nightly Rust toolchain
- Instrumented builds are 2-3x larger
- TSan has significant memory overhead

### Benchmarks (`cargo bench`)
- Results are meaningless on shared/variable cloud hardware
- Need dedicated hardware with consistent CPU frequency
- Production benchmarks run on c7i.2xlarge (AWS Mumbai)

## Cloud Environment Limits

| Resource | GitHub Actions | Claude Code Web |
|----------|---------------|-----------------|
| Disk | 14GB free | ~29GB free |
| RAM | 7GB | Variable |
| CPU | 2 cores (shared) | Variable |
| Timeout | 6 hours | Session-based |

## Recommended Workflow

### Daily Development (Cloud)
```bash
cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace
```

### Pre-Push (Cloud)
```bash
make check  # fmt + clippy + test
```

### Weekly Quality (Local)
```bash
make coverage      # llvm-cov with 95% threshold
make bench         # benchmark budgets
cargo mutants      # mutation testing
```

### Monthly Security (Local)
```bash
cargo +nightly careful test
RUSTFLAGS="-Zsanitizer=address" cargo +nightly test
RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test
cargo fuzz run tick_parser -- -max_total_time=3600
```
