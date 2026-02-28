# Quality Gates

## Before Every Commit
1. `cargo fmt` — format all code
2. `cargo clippy -- -D warnings` — zero warnings
3. `cargo test` — 100% pass

## Coverage Thresholds
- core/trading crates: 95%
- common/storage/api crates: 90%
- app/integration: 80%

## Test Naming
```
fn test_<module>_<function>_<scenario>_<expected_outcome>()
```

## CI Pipeline Order
Compile -> Lint -> Test -> Security -> Performance -> Coverage
Any failure = RED. No exceptions.

## Error Handling
- Fix root cause, never suppress
- Never `#[allow(...)]` without Parthiban's approval
- No `.unwrap()` in production code — use `?` with anyhow/thiserror

## Adding Dependencies
1. Check Bible for approved version
2. Not in Bible? Propose to Parthiban with justification
3. Use exact version only (no `^`, `~`, `*`)

## Documentation
- Every `pub fn/struct/enum/trait` gets doc comment (WHAT and WHY)
- Hot-path items: include `# Performance` section
- `cargo doc --no-deps` must build with zero warnings
