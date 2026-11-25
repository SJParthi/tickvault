# November 25 market-session verification

The November 25 Stocks capture showed 20 eligible F&O losers in ascending change-percent order and 20 eligible Nifty 50 gainers in descending order. At 09:52 IST the service was running main build `9ccd99e7`. Runtime evidence showed paper orders and disabled depth sockets.

At reviewed source `ca905eec6`, eight hosted test jobs passed 18,654 tests and skipped 39. Whole-workspace production line coverage was 117,589 of 139,576. The hosted run exercised all 61 changed executable production Rust lines. Each of three four-quote frame paths exceeded one microsecond at p99, measuring 1.206, 2.446 and 2.438 µs, respectively. The operator API measured 2,533.879 ms p99 across 14 requests in one five-minute production window.

Separate local tests found that the scheduled audit discarded an unacknowledged daily receipt and that the feed scoreboard could report complete with missing audit counters. Commit `18537fe00` journals the receipt before delivery. Ten recovery cases finished with one database row and a second restart sent zero copies. When disk and database both refused the row, the eleventh case reported zero confirmed copies. Commit `b9d90717a` checks five producer counters and two writer stages. Its 31 targeted scenarios distinguished complete, partial and degraded evidence. The package records the test revisions and open limitations.

Download and unpack the archive. Open `comparison.html` for the results table or play `tickvault-measurements.mp4` for the narrated walkthrough. Screenshots, captions, raw measurements, local reproduction projects, the historical branch review, checksums and Claude Code continuation instructions are included. The recovery and scoreboard changes are available for review on this branch. The live portal ran the earlier build at capture.

[Download the video and evidence](market-session-verification.zip) · [Hosted CI](https://github.com/SJParthi/tickvault/actions/runs/45539365285) · [Exact benchmark](https://github.com/SJParthi/tickvault/actions/runs/45546320350/job/138976706728)
