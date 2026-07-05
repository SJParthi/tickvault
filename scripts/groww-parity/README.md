# Groww Parity Comparer (PR-R2)

Offline comparer for the two Groww live-tick capture files — the Python
sidecar (`data/groww/live-ticks.ndjson`) vs the native Rust shadow client
(`data/groww/rust-live-ticks.ndjson`). Authorized by
`.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §35
("PR-R2 = parity comparer").

## What it answers

1. **Parity** — which side missed which ticks: total per side, matched,
   missing-in-rust, missing-in-python (each split index vs stock).
2. **Latency** — per-tick receipt-latency delta
   `python_capture_ns - rust_capture_ns` with p50/p90/p99/min/max/mean.
   Positive = Python captured later (Rust was faster). Both stamps come
   from the SAME host wall clock, so deltas are host-clock-consistent.

Join key: `(segment, security_id, exchange_ts_millis)` — there is no
separate `exchange` field in the line schema; exchange identity is folded
into the canonical segment string.

## Run

```bash
python3 scripts/groww-parity/compare.py
# or with explicit paths:
python3 scripts/groww-parity/compare.py \
    --python data/groww/live-ticks.ndjson \
    --rust data/groww/rust-live-ticks.ndjson
```

Outputs: human table on stdout, summary TSV at
`data/groww/parity-<IST-date>.tsv`, and a Telegram-ready 5-line summary
at the end. `--no-tsv` skips the file.

## Guarantees / honest envelope

- Streams both files line-by-line (never slurps); memory is O(unique
  keys) — fine at 800K+ lines, honestly O(N-keys), not O(1).
- Dedupes within each side first (duplicates counted per side).
- Malformed / blank / unjoinable (`exchange_ts_millis <= 0`) lines are
  counted + sampled, never crash the run.
- Python lines without `capture_ns` (old format) still count for parity
  but are excluded from latency stats (`matched_no_latency`).
- Measures only ticks PRESENT in the capture files — upstream ticks Groww
  never delivered to either client are invisible (CAPTURE vs UPSTREAM
  split, groww-second-feed-scope §5).

## Tests

```bash
cd scripts/groww-parity && python3 -m unittest test_compare -v
```

22 tests over synthetic fixtures: perfect match, rust-missing-N,
python-missing-N, exact percentile assertions, negative deltas, malformed
lines, duplicate keys, absent capture_ns, class split, ts==0 exclusion,
TSV content, absent-file handling, renderer smoke, end-to-end exit codes.
