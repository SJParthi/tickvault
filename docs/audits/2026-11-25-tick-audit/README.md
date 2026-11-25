# Tick audit and Stocks verification, November 25, 2026

When the app accepts 100 updates and the database returns zero, a spill counter of 100 can explain a possible delay. The dashboard marks the result unverified until a fresh database read agrees. A count match alone cannot identify individual missing or duplicate ticks.

| What was checked | Result | Where it was measured |
| --- | --- | --- |
| Two audit receipts, each sent twice | Two rows remained after restart. All seven checked fields matched. | Production Rust writer, isolated QuestDB 9.3.5 |
| Changed production Rust lines at revision `75637a5be` | 61 of 61 exercised across three files | [Local LLVM instrumentation](changed-lines.json). The parser was rebuilt. The dashboard and classifier matched their instrumented sources byte for byte. |
| Whole-workspace production lines at `9ccd99e7` | 117,496 of 139,557 covered (84.19%) | [Hosted coverage](https://github.com/SJParthi/tickvault/actions/runs/45461479036/job/138686672306) |
| Time for one frame containing four quotes | p99 1.833 µs standard, 3.757 µs full library, 3.717 µs full custom | 65,536 frames per path on the [hosted benchmark](https://github.com/SJParthi/tickvault/actions/runs/45461485528/job/138684939224) |
| Warm paper-order placement | p99 0.751 µs inside the app | Same hosted benchmark |
| Constructing a labelled database line | p99 1.342 µs before network and storage | Same hosted benchmark |
| Production Stocks page at 08:10 IST on November 25 | Price Movers, Gainers/Losers, NSE and F&O Stocks rendered. Binary, portal and main reported `9ccd99e`. The instance was stopped. | [Authenticated page capture](stocks.png) |

[Watch the narrated comparison](measurements.mp4) · [English captions](captions.vtt)

## Branch inventory before publication

The [branch inventory](branch-inventory.tsv) covers 925 non-main branch names at 893 distinct tips. Each row records the exact SHA, related PRs and how its changes compare with main. The combined branch contains the dependency head from [PR #2289](https://github.com/SJParthi/tickvault/pull/2289). The Stocks rebuild reached main through [PR #2187](https://github.com/SJParthi/tickvault/pull/2187).

The historical `claude/integration` branch would change 64 files in a clean Git merge, including restoring retired Groww and volume-ranking modules and duplicating a Rust import. Review any still-needed changes individually. Main already includes the later feed scan work and the ported WAL acknowledgement logic. [Branch comparison](https://github.com/SJParthi/tickvault/compare/9ccd99e7...82ed11e7) · [WAL decision](https://github.com/SJParthi/tickvault/blob/9ccd99e7/.claude/plans/active-plan-audit-2026-10-20.md#L723-L733)

## Continue the review in Claude Code

Preserve existing branches and compare this PR's head with current main. Verify the dependency head from PR #2289 is an ancestor. Run the repository's full checks on the combined head and check the hosted app test for runner disk exhaustion. Reproduce the missing-count, delayed-read, malformed-QuestDB and partial-benchmark fixtures. Confirm the expected audit reason and the stored row after restart. Check coverage, p99 and the benchmark cache verdict with their units and workload. Report observed failures and the exact tested commit.

When the service is running, open the authenticated Stocks page and verify real dated rows through F&O and index filters, both mover directions and percent sorting. Cross-check displayed data with its read-only source. Keep Depth20, Depth200 and live order activation off. Production timing, per-tick identity, recovery after failed final audit writes and comparison with a live broker each need separate evidence.
