# Tick audit and Stocks verification, November 25, 2026

When the app accepts 100 updates and the database returns zero, a spill counter of 100 can explain a possible delay. The dashboard marks the result unverified until a fresh database read agrees. A count match alone cannot identify individual missing or duplicate ticks.

| What was checked | Result | Where it was measured |
| --- | --- | --- |
| Two audit receipts, each sent twice | Two rows remained after restart. All seven checked fields matched. | Production Rust writer, isolated QuestDB 9.3.5 |
| Changed production Rust lines at revision `16212ad77` | 51 of 51 exercised across three files | Local LLVM instrumentation |
| Whole-workspace production lines at `9ccd99e7` | 117,496 of 139,557 covered (84.19%) | [Hosted coverage](https://github.com/SJParthi/tickvault/actions/runs/45461479036/job/138686672306) |
| Time for one frame containing four quotes | p99 1.833 µs standard, 3.757 µs full library, 3.717 µs full custom | 65,536 frames per path on the [hosted benchmark](https://github.com/SJParthi/tickvault/actions/runs/45461485528/job/138684939224) |
| Warm paper-order placement | p99 0.751 µs inside the app | Same hosted benchmark |
| Constructing a labelled database line | p99 1.342 µs before network and storage | Same hosted benchmark |
| Production Stocks page at 08:10 IST on November 25 | Price Movers, Gainers/Losers, NSE and F&O Stocks rendered. Binary, portal and main reported `9ccd99e`. The instance was stopped. | [Authenticated page capture](stocks.png) |

[Watch the narrated comparison](measurements.mp4) · [English captions](captions.vtt)

## Continue the review in Claude Code

Preserve existing branches and compare this PR's head with current main. Verify the dependency head from PR #2289 is an ancestor. Run the repository's full checks on the combined head and check the hosted app test for runner disk exhaustion. Reproduce the missing-count, delayed-read, malformed-QuestDB and partial-benchmark fixtures. Confirm the expected audit reason and the stored row after restart. Check coverage, p99 and the benchmark cache verdict with their units and workload. Report observed failures and the exact tested commit.

When the service is running, open the authenticated Stocks page and verify real dated rows through F&O and index filters, both mover directions and percent sorting. Cross-check displayed data with its read-only source. Keep Depth20, Depth200 and live order activation off. Production timing, per-tick identity, recovery after failed final audit writes and comparison with a live broker each need separate evidence.
