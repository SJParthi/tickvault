# Follow-up verification and narrated Stocks comparison

The production Stocks captures show the service on `9ccd99e7`. The original draft PR was benchmarked on `ca905eec6`. Both audit repairs were tested together on `b6cb4ef0d`. GitHub completed that check at 11:14 IST on November 25, with 18,659 tests passed, 39 skipped and production line coverage of 117,800/139,698 (84.32%).

Open the downloadable archive and play `viewer/tickvault-measurements.mp4`, or open `viewer/comparison.html` for the offline comparison. The archive includes the independent Stocks finite model, the four-page incremental ranking experiment, the original and journaled receipt latency distributions, GitHub receipts, the production socket query and the current handoff instructions. The earlier archive contains the source and results for the production selector benchmark and the eleven crash/restart cases.

[Download the latest narrated video, tables and test artifacts](repair-verification.zip) · [Earlier market-session evidence](market-session-verification.md) · [Exact follow-up CI](https://github.com/SJParthi/tickvault/actions/runs/45552287623)

Hosted LLVM hit 132/148 executable production lines introduced by the follow-up and 193/209 for the combined changes relative to main. Sixteen changed lines still lack hosted line coverage. The standalone comparison also documents the 5,827,536 finite-model and page-boundary comparisons, the incremental prototype timings, and the disk-backed receipt latency before and after journaling.
