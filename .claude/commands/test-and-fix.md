Run the full test suite. If any tests fail, analyze the failures and fix them. Repeat until green.

## Steps

1. Run `cargo test --workspace 2>&1` and capture output
2. If all tests pass, report success and stop
3. If tests fail:
   a. Analyze each failure — read the failing test and the code it tests
   b. Determine if the fix is in the test or the implementation
   c. Fix the issue
   d. Re-run `cargo test --workspace`
   e. Repeat until all tests pass
4. After green: run `cargo clippy --workspace --all-targets -- -D warnings` to ensure no new warnings

## Rules

- Never delete or skip failing tests to make the suite pass
- Never use `#[ignore]` to hide failures
- If a test is genuinely wrong, fix the test — explain why in a comment
- Maximum 3 fix-and-retry cycles. If still failing after 3, report the issue and stop
