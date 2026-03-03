Stage all changed files, create a conventional commit, and push to the current branch.

## Steps

1. Run `git status` to see what changed
2. Run `git diff --stat` to understand the scope
3. Stage relevant files (prefer specific files over `git add -A`)
4. Write a conventional commit message following the format: `<type>(<scope>): <description>`
   - Types: feat, fix, refactor, test, docs, chore, perf, security
   - Scope: the crate or area changed (e.g., core, trading, app, hooks)
   - Description: concise, imperative mood, lowercase
5. Commit the changes
6. Push to the current branch with `-u origin`
7. Show the result with `git log --oneline -1`

## Rules

- Never commit .env files, secrets, or credentials
- Never use `git add -A` blindly — review what's staged
- One logical change per commit
- Every commit must compile (`cargo check` should have passed)
