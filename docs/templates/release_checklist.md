# Release Checklist — Before Every Version Bump

> Extracted from CLAUDE.md Gate 5b. Claude Code fills this out, Parthiban reviews.

## Pre-Release
- [ ] All CI gates pass on `develop` (6 stages green)
- [ ] All benchmarks within budget (zero regressions >5%)
- [ ] Coverage meets per-crate thresholds
- [ ] cargo audit: zero known CVEs
- [ ] No TODO/FIXME/HACK in production code (grep verified)
- [ ] CHANGELOG.md updated with all changes since last release
- [ ] Version bumped in workspace Cargo.toml

## Build & Tag
- [ ] Merge develop → main via PR (with CI passing)
- [ ] Tag: `git tag -a v<X.Y.Z> -m "Release v<X.Y.Z>: <summary>"`
- [ ] Docker image built: `docker build -t dlt:v<X.Y.Z> .`
- [ ] Docker image tagged: `dlt:v<X.Y.Z>` (NEVER :latest)
- [ ] Push tag to GitHub: `git push origin v<X.Y.Z>`

## Deploy
- [ ] Traefik blue-green: new version behind canary route
- [ ] Smoke test passes on canary
- [ ] Swap traffic to new version
- [ ] Old version running for 30 minutes (instant rollback window)
- [ ] All Gate 6 runtime checks pass

## Post-Release
- [ ] Telegram notification: "v<X.Y.Z> deployed successfully"
- [ ] Grafana annotation: release marker on dashboards
- [ ] Old version containers stopped after 30-minute window
- [ ] Git tag pushed, GitHub release created with changelog
