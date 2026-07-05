#!/usr/bin/env python3
"""Unit tests for the PR-R2 parity comparer (synthetic NDJSON fixtures).

Run:
    cd scripts/groww-parity && python3 -m unittest test_compare -v
"""

from __future__ import annotations

import json
import os
import tempfile
import unittest

import compare


def tick(security_id, segment, ts_ms, ltp, capture_ns=None):
    rec = {
        "security_id": security_id,
        "segment": segment,
        "ts_ist_nanos": ts_ms * 1_000_000 + 19_800_000_000_000,
        "exchange_ts_millis": ts_ms,
        "ltp": ltp,
        "cum_volume": 0,
    }
    if capture_ns is not None:
        rec["capture_ns"] = capture_ns
    return json.dumps(rec)


BASE_MS = 1_782_793_353_000
BASE_NS = BASE_MS * 1_000_000


class ParityTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.py_path = os.path.join(self.tmp.name, "live-ticks.ndjson")
        self.rust_path = os.path.join(self.tmp.name, "rust-live-ticks.ndjson")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, path, lines):
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("\n".join(lines) + ("\n" if lines else ""))

    def run_compare(self, py_lines, rust_lines):
        self.write(self.py_path, py_lines)
        self.write(self.rust_path, rust_lines)
        return compare.compare(self.py_path, self.rust_path)

    # -- 1. perfect match ---------------------------------------------------
    def test_perfect_match(self):
        n = 50
        py, rust = [], []
        for i in range(n):
            ts = BASE_MS + i * 1000
            py.append(tick(2885, "NSE_EQ", ts, 100.0 + i, BASE_NS + i * 1_000_000))
            rust.append(tick(2885, "NSE_EQ", ts, 100.0 + i, BASE_NS + i * 1_000_000))
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched"], n)
        self.assertEqual(r["missing_in_rust"], 0)
        self.assertEqual(r["missing_in_python"], 0)
        self.assertEqual(r["python_unique"], n)
        self.assertEqual(r["rust_unique"], n)
        self.assertEqual(r["ltp_mismatch"], 0)
        # identical capture stamps -> every delta exactly 0
        self.assertEqual(r["latency_all"]["count"], n)
        self.assertEqual(r["latency_all"]["min"], 0)
        self.assertEqual(r["latency_all"]["max"], 0)

    # -- 2. rust missing N --------------------------------------------------
    def test_rust_missing(self):
        py = [
            tick(13, "IDX_I", BASE_MS + i * 1000, 25000.0, BASE_NS) for i in range(10)
        ]
        rust = [tick(13, "IDX_I", BASE_MS + i * 1000, 25000.0, BASE_NS) for i in range(7)]
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched"], 7)
        self.assertEqual(r["missing_in_rust"], 3)
        self.assertEqual(r["missing_in_python"], 0)
        self.assertEqual(r["missing_in_rust_by_class"]["index"], 3)
        self.assertEqual(r["missing_in_rust_by_class"]["stock"], 0)

    # -- 3. python missing N ------------------------------------------------
    def test_python_missing(self):
        py = [tick(2885, "NSE_EQ", BASE_MS + i * 1000, 1.0, BASE_NS) for i in range(4)]
        rust = [
            tick(2885, "NSE_EQ", BASE_MS + i * 1000, 1.0, BASE_NS) for i in range(9)
        ]
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched"], 4)
        self.assertEqual(r["missing_in_python"], 5)
        self.assertEqual(r["missing_in_rust"], 0)
        self.assertEqual(r["missing_in_python_by_class"]["stock"], 5)

    # -- 4. known latency deltas -> exact percentiles ------------------------
    def test_latency_percentiles(self):
        # deltas: 1ms..10ms (python captured later by i ms)
        py, rust = [], []
        for i in range(1, 11):
            ts = BASE_MS + i * 1000
            rust.append(tick(2885, "NSE_EQ", ts, 5.0, BASE_NS))
            py.append(tick(2885, "NSE_EQ", ts, 5.0, BASE_NS + i * 1_000_000))
        r = self.run_compare(py, rust)
        s = r["latency_all"]
        self.assertEqual(s["count"], 10)
        self.assertEqual(s["min"], 1_000_000)
        self.assertEqual(s["max"], 10_000_000)
        self.assertEqual(s["p50"], 5_000_000)  # nearest-rank: 5th of 10
        self.assertEqual(s["p90"], 9_000_000)
        self.assertEqual(s["p99"], 10_000_000)
        self.assertAlmostEqual(s["mean"], 5_500_000.0)

    # -- 4b. negative delta (rust captured later) preserved ------------------
    def test_negative_latency_preserved(self):
        py = [tick(2885, "NSE_EQ", BASE_MS, 5.0, BASE_NS)]
        rust = [tick(2885, "NSE_EQ", BASE_MS, 5.0, BASE_NS + 2_000_000)]
        r = self.run_compare(py, rust)
        self.assertEqual(r["latency_all"]["min"], -2_000_000)
        self.assertEqual(r["latency_all"]["max"], -2_000_000)

    # -- 5. malformed lines counted, never crash -----------------------------
    def test_malformed_lines(self):
        py = [
            tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS),
            "{not json",
            '"a bare string"',
            json.dumps({"security_id": "x", "segment": "NSE_EQ",
                        "exchange_ts_millis": BASE_MS, "ltp": 1.0}),
            "",
            tick(1, "NSE_EQ", BASE_MS + 1000, 1.0, BASE_NS),
        ]
        rust = [
            tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS),
            "garbage line",
            tick(1, "NSE_EQ", BASE_MS + 1000, 1.0, BASE_NS),
        ]
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched"], 2)
        self.assertEqual(r["python_stats"].malformed, 3)
        self.assertEqual(r["python_stats"].blank, 1)
        self.assertEqual(r["rust_stats"].malformed, 1)
        self.assertTrue(r["python_stats"].malformed_samples)

    # -- 6. duplicate keys deduped + counted ----------------------------------
    def test_duplicate_keys(self):
        dup = tick(2885, "NSE_EQ", BASE_MS, 7.0, BASE_NS)
        py = [dup, dup, dup, tick(2885, "NSE_EQ", BASE_MS + 1000, 8.0, BASE_NS)]
        rust = [dup, dup]
        r = self.run_compare(py, rust)
        self.assertEqual(r["python_unique"], 2)
        self.assertEqual(r["rust_unique"], 1)
        self.assertEqual(r["python_stats"].duplicates, 2)
        self.assertEqual(r["rust_stats"].duplicates, 1)
        self.assertEqual(r["matched"], 1)
        self.assertEqual(r["missing_in_rust"], 1)

    # -- 7. python line without capture_ns: matched, no latency --------------
    def test_missing_capture_ns(self):
        py = [
            tick(13, "IDX_I", BASE_MS, 25000.0),  # no capture_ns
            tick(13, "IDX_I", BASE_MS + 1000, 25001.0, BASE_NS),
        ]
        rust = [
            tick(13, "IDX_I", BASE_MS, 25000.0, BASE_NS),
            tick(13, "IDX_I", BASE_MS + 1000, 25001.0, BASE_NS),
        ]
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched"], 2)
        self.assertEqual(r["matched_no_latency"], 1)
        self.assertEqual(r["latency_all"]["count"], 1)
        self.assertEqual(r["python_stats"].no_capture_ns, 1)

    # -- 8. per-class split (IDX_I vs stock) ----------------------------------
    def test_class_split(self):
        py = [
            tick(13, "IDX_I", BASE_MS, 25000.0, BASE_NS + 3_000_000),
            tick(2885, "NSE_EQ", BASE_MS, 100.0, BASE_NS + 7_000_000),
        ]
        rust = [
            tick(13, "IDX_I", BASE_MS, 25000.0, BASE_NS),
            tick(2885, "NSE_EQ", BASE_MS, 100.0, BASE_NS),
        ]
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched_by_class"]["index"], 1)
        self.assertEqual(r["matched_by_class"]["stock"], 1)
        self.assertEqual(r["latency_by_class"]["index"]["p50"], 3_000_000)
        self.assertEqual(r["latency_by_class"]["stock"]["p50"], 7_000_000)

    # -- 9. unjoinable ts==0 excluded ------------------------------------------
    def test_unjoinable_ts_zero(self):
        py = [
            tick(1, "NSE_EQ", 0, 1.0, BASE_NS),
            tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS),
        ]
        rust = [tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS)]
        r = self.run_compare(py, rust)
        self.assertEqual(r["python_stats"].unjoinable, 1)
        self.assertEqual(r["python_unique"], 1)
        self.assertEqual(r["matched"], 1)
        self.assertEqual(r["missing_in_rust"], 0)

    # -- 10. TSV output written with expected rows -----------------------------
    def test_tsv_output(self):
        py = [tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS + 1_000_000)]
        rust = [tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS)]
        r = self.run_compare(py, rust)
        path = compare.write_tsv(r, self.tmp.name, "2026-07-05")
        self.assertTrue(path.endswith("parity-2026-07-05.tsv"))
        with open(path, encoding="utf-8") as fh:
            rows = dict(
                line.rstrip("\n").split("\t", 1) for line in fh if line.strip()
            )
        self.assertEqual(rows["matched"], "1")
        self.assertEqual(rows["missing_in_rust"], "0")
        self.assertEqual(rows["latency_all_p50_ns"], "1000000")

    # -- extra: ltp mismatch on matched key counted ---------------------------
    def test_ltp_mismatch_counted(self):
        py = [tick(1, "NSE_EQ", BASE_MS, 100.5, BASE_NS)]
        rust = [tick(1, "NSE_EQ", BASE_MS, 100.6, BASE_NS)]
        r = self.run_compare(py, rust)
        self.assertEqual(r["matched"], 1)
        self.assertEqual(r["ltp_mismatch"], 1)
        self.assertTrue(r["ltp_mismatch_samples"])

    # -- extra: one side absent — honest report, no crash ---------------------
    def test_rust_file_absent(self):
        self.write(self.py_path, [tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS)])
        r = compare.compare(self.py_path, self.rust_path)
        self.assertFalse(r["rust_exists"])
        self.assertEqual(r["python_unique"], 1)
        self.assertEqual(r["missing_in_rust"], 1)
        self.assertEqual(r["matched"], 0)

    # -- extra: renderers never crash on empty/degenerate results -------------
    def test_renderers_smoke(self):
        r = self.run_compare([], [])
        report = compare.render_report(r)
        self.assertIn("GROWW PARITY", report)
        self.assertIn("SAME host wall clock", report)
        summary = compare.render_telegram_summary(r)
        self.assertEqual(len(summary.splitlines()), 5)

    # -- extra: main() end-to-end exit codes ----------------------------------
    def test_main_end_to_end(self):
        self.write(self.py_path, [tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS)])
        self.write(self.rust_path, [tick(1, "NSE_EQ", BASE_MS, 1.0, BASE_NS)])
        rc = compare.main(
            [
                "--python", self.py_path,
                "--rust", self.rust_path,
                "--tsv-dir", self.tmp.name,
            ]
        )
        self.assertEqual(rc, 0)

    def test_main_both_inputs_absent(self):
        rc = compare.main(
            [
                "--python", os.path.join(self.tmp.name, "nope-a"),
                "--rust", os.path.join(self.tmp.name, "nope-b"),
                "--no-tsv",
            ]
        )
        self.assertEqual(rc, 2)


class PercentileTests(unittest.TestCase):
    def test_empty(self):
        self.assertIsNone(compare.percentile([], 50))

    def test_single(self):
        self.assertEqual(compare.percentile([42], 50), 42)
        self.assertEqual(compare.percentile([42], 99), 42)

    def test_nearest_rank_exact_boundary(self):
        # 10 values, p30 -> ceil(3.0) = 3rd element (banker's-rounding trap)
        vals = list(range(1, 11))
        self.assertEqual(compare.percentile(vals, 30), 3)
        self.assertEqual(compare.percentile(vals, 50), 5)
        self.assertEqual(compare.percentile(vals, 100), 10)
        self.assertEqual(compare.percentile(vals, 0), 1)


class ParseLineTests(unittest.TestCase):
    def test_bool_security_id_rejected(self):
        kind, _ = compare.parse_line(
            json.dumps({"security_id": True, "segment": "NSE_EQ",
                        "exchange_ts_millis": BASE_MS, "ltp": 1.0})
        )
        self.assertEqual(kind, "malformed")

    def test_garbage_capture_ns_treated_absent(self):
        kind, payload = compare.parse_line(
            json.dumps({"security_id": 1, "segment": "NSE_EQ",
                        "exchange_ts_millis": BASE_MS, "ltp": 1.0,
                        "capture_ns": "oops"})
        )
        self.assertEqual(kind, "ok")
        self.assertIsNone(payload[1])

    def test_negative_ts_unjoinable(self):
        kind, _ = compare.parse_line(
            json.dumps({"security_id": 1, "segment": "NSE_EQ",
                        "exchange_ts_millis": -5, "ltp": 1.0})
        )
        self.assertEqual(kind, "unjoinable")


if __name__ == "__main__":
    unittest.main()
