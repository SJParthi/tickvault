#!/usr/bin/env python3
"""PR-R2 — Groww Python-sidecar vs native-Rust tick parity + latency comparer.

Authorized by .claude/rules/project/groww-second-feed-scope-2026-06-19.md §35
("PR-R2 = parity comparer"). Offline analysis tool — never on any hot path,
never touches the live feed, tokens, or QuestDB.

INPUTS (both NDJSON, identical GrowwTickLine schema — verified field-for-field
against the two writers):
  Python side: data/groww/live-ticks.ndjson
    writer: scripts/groww-sidecar/groww_sidecar.py::_write_record
  Rust side:   data/groww/rust-live-ticks.ndjson
    writer: crates/core/src/feed/groww/native/shadow_writer.rs::format_ndjson_line

Line schema (both sides):
  {"security_id": int, "segment": str, "ts_ist_nanos": int,
   "exchange_ts_millis": int, "ltp": float, "cum_volume": 0,
   "capture_ns": int}            # capture_ns OPTIONAL on the Python side

There is NO separate "exchange" field — exchange identity is folded into the
canonical `segment` string (NSE_EQ, IDX_I, ...). The join key is therefore
(segment, security_id, exchange_ts_millis) — the honest realization of the
requested (exchange, segment, token, exchange_timestamp_ms) key.

CLOCK BASIS (stated in every report header): both `capture_ns` stamps come
from the SAME host wall clock — `time.time_ns()` in the Python sidecar and
`SystemTime` in the Rust shadow client — so per-tick deltas
(python_capture_ns - rust_capture_ns) are host-clock-consistent. A positive
delta means Python captured LATER (Rust was faster).

MEMORY: streaming line-by-line (never slurps a file); the join holds one
dict of unique Rust keys + one Python seen-set. That is O(unique keys) —
tens of MB at 800K+ lines — honestly O(N-keys), not O(1).

Usage:
    python3 scripts/groww-parity/compare.py \
        [--python data/groww/live-ticks.ndjson] \
        [--rust data/groww/rust-live-ticks.ndjson] \
        [--tsv-dir data/groww] [--no-tsv]

Run tests:
    cd scripts/groww-parity && python3 -m unittest test_compare -v
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from datetime import datetime, timedelta, timezone

DEFAULT_PYTHON_PATH = "data/groww/live-ticks.ndjson"
DEFAULT_RUST_PATH = "data/groww/rust-live-ticks.ndjson"
DEFAULT_TSV_DIR = "data/groww"

IST = timezone(timedelta(hours=5, minutes=30))
INDEX_SEGMENT = "IDX_I"
MALFORMED_SAMPLE_CAP = 3
LTP_MISMATCH_SAMPLE_CAP = 3
# Float tolerance for "same tick, same price" (both writers print the same
# f64; tolerance only guards textual float round-trip).
LTP_EPSILON = 1e-9


def parse_line(raw: str):
    """Parse one NDJSON line -> (key, capture_ns_or_None, ltp) or a reject.

    Returns a tuple (kind, payload):
      ("ok", (key, capture_ns, ltp))     key = (segment, security_id, ts_ms)
      ("unjoinable", None)               exchange_ts_millis missing/0
      ("malformed", reason_str)          bad JSON / wrong shape / wrong types
      ("blank", None)                    empty/whitespace-only line
    Never raises.
    """
    stripped = raw.strip()
    if not stripped:
        return ("blank", None)
    try:
        obj = json.loads(stripped)
    except (json.JSONDecodeError, ValueError):
        return ("malformed", "bad json")
    if not isinstance(obj, dict):
        return ("malformed", "not an object")
    try:
        segment = obj["segment"]
        security_id = obj["security_id"]
        ts_ms = obj["exchange_ts_millis"]
        ltp = obj["ltp"]
        if not isinstance(segment, str):
            return ("malformed", "segment not str")
        if isinstance(security_id, bool) or not isinstance(security_id, int):
            return ("malformed", "security_id not int")
        if isinstance(ts_ms, bool) or not isinstance(ts_ms, int):
            return ("malformed", "exchange_ts_millis not int")
        ltp = float(ltp)
    except (KeyError, TypeError, ValueError):
        return ("malformed", "missing/invalid field")
    if ts_ms <= 0:
        return ("unjoinable", None)
    capture_ns = obj.get("capture_ns")
    if capture_ns is not None and (
        isinstance(capture_ns, bool) or not isinstance(capture_ns, int)
    ):
        capture_ns = None  # treat a garbage stamp as absent, never crash
    return ("ok", ((segment, security_id, ts_ms), capture_ns, ltp))


class SideStats:
    """Per-side streaming counters (no line retention)."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.total_lines = 0
        self.parsed = 0
        self.malformed = 0
        self.malformed_samples: list[str] = []
        self.blank = 0
        self.unjoinable = 0
        self.duplicates = 0
        self.no_capture_ns = 0
        self.first_ts_ms: int | None = None
        self.last_ts_ms: int | None = None

    def note_tick(self, ts_ms: int) -> None:
        if self.first_ts_ms is None or ts_ms < self.first_ts_ms:
            self.first_ts_ms = ts_ms
        if self.last_ts_ms is None or ts_ms > self.last_ts_ms:
            self.last_ts_ms = ts_ms

    def note_reject(self, kind: str, raw: str, reason) -> None:
        if kind == "blank":
            self.blank += 1
        elif kind == "unjoinable":
            self.unjoinable += 1
        else:
            self.malformed += 1
            if len(self.malformed_samples) < MALFORMED_SAMPLE_CAP:
                snippet = raw.strip()[:120]
                self.malformed_samples.append(f"{reason}: {snippet}")


def percentile(sorted_vals: list, pct: float):
    """Nearest-rank percentile over a pre-sorted list. None on empty."""
    if not sorted_vals:
        return None
    if pct <= 0:
        return sorted_vals[0]
    if pct >= 100:
        return sorted_vals[-1]
    rank = max(1, math.ceil(pct / 100.0 * len(sorted_vals)))
    rank = min(rank, len(sorted_vals))
    return sorted_vals[rank - 1]


def latency_summary(deltas_ns: list) -> dict:
    """p50/p90/p99/min/max/mean over latency deltas (ns). Empty-safe."""
    if not deltas_ns:
        return {"count": 0}
    vals = sorted(deltas_ns)
    return {
        "count": len(vals),
        "min": vals[0],
        "max": vals[-1],
        "mean": sum(vals) / len(vals),
        "p50": percentile(vals, 50),
        "p90": percentile(vals, 90),
        "p99": percentile(vals, 99),
    }


def classify(segment: str) -> str:
    """index (IDX_I) vs stock (everything else)."""
    return "index" if segment == INDEX_SEGMENT else "stock"


def load_rust_side(path: str):
    """Stream the Rust NDJSON -> (key -> (capture_ns, ltp)), SideStats.

    First occurrence of a key wins; later duplicates are counted only.
    """
    stats = SideStats("rust")
    keyed: dict = {}
    if not os.path.exists(path):
        return keyed, stats, False
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            stats.total_lines += 1
            kind, payload = parse_line(raw)
            if kind != "ok":
                stats.note_reject(kind, raw, payload)
                continue
            key, capture_ns, ltp = payload
            stats.parsed += 1
            stats.note_tick(key[2])
            if capture_ns is None:
                stats.no_capture_ns += 1
            if key in keyed:
                stats.duplicates += 1
                continue
            keyed[key] = (capture_ns, ltp)
    return keyed, stats, True


def compare(python_path: str, rust_path: str):
    """Core join. Returns a result dict (pure data — rendering is separate)."""
    rust_keyed, rust_stats, rust_exists = load_rust_side(rust_path)

    py_stats = SideStats("python")
    py_exists = os.path.exists(python_path)
    py_seen: set = set()
    matched_keys: set = set()
    missing_in_rust = 0
    missing_in_rust_by_class = {"index": 0, "stock": 0}
    matched_by_class = {"index": 0, "stock": 0}
    matched_no_latency = 0  # matched but a capture_ns absent on either side
    ltp_mismatch = 0
    ltp_mismatch_samples: list[str] = []
    deltas_all: list = []
    deltas_by_class: dict = {"index": [], "stock": []}

    if py_exists:
        with open(python_path, "r", encoding="utf-8", errors="replace") as fh:
            for raw in fh:
                py_stats.total_lines += 1
                kind, payload = parse_line(raw)
                if kind != "ok":
                    py_stats.note_reject(kind, raw, payload)
                    continue
                key, py_capture_ns, py_ltp = payload
                py_stats.parsed += 1
                py_stats.note_tick(key[2])
                if py_capture_ns is None:
                    py_stats.no_capture_ns += 1
                if key in py_seen:
                    py_stats.duplicates += 1
                    continue
                py_seen.add(key)
                cls = classify(key[0])
                rust_entry = rust_keyed.get(key)
                if rust_entry is None:
                    missing_in_rust += 1
                    missing_in_rust_by_class[cls] += 1
                    continue
                matched_keys.add(key)
                matched_by_class[cls] += 1
                rust_capture_ns, rust_ltp = rust_entry
                if abs(py_ltp - rust_ltp) > LTP_EPSILON:
                    ltp_mismatch += 1
                    if len(ltp_mismatch_samples) < LTP_MISMATCH_SAMPLE_CAP:
                        ltp_mismatch_samples.append(
                            f"{key}: py={py_ltp} rust={rust_ltp}"
                        )
                if py_capture_ns is None or rust_capture_ns is None:
                    matched_no_latency += 1
                    continue
                delta = py_capture_ns - rust_capture_ns
                deltas_all.append(delta)
                deltas_by_class[cls].append(delta)

    # Rust keys never matched by a Python line = missing-in-python.
    missing_in_python = 0
    missing_in_python_by_class = {"index": 0, "stock": 0}
    for key in rust_keyed:
        if key not in matched_keys:
            missing_in_python += 1
            missing_in_python_by_class[classify(key[0])] += 1

    return {
        "python_exists": py_exists,
        "rust_exists": rust_exists,
        "python_stats": py_stats,
        "rust_stats": rust_stats,
        "python_unique": len(py_seen),
        "rust_unique": len(rust_keyed),
        "matched": len(matched_keys),
        "matched_by_class": matched_by_class,
        "matched_no_latency": matched_no_latency,
        "missing_in_rust": missing_in_rust,
        "missing_in_rust_by_class": missing_in_rust_by_class,
        "missing_in_python": missing_in_python,
        "missing_in_python_by_class": missing_in_python_by_class,
        "ltp_mismatch": ltp_mismatch,
        "ltp_mismatch_samples": ltp_mismatch_samples,
        "latency_all": latency_summary(deltas_all),
        "latency_by_class": {
            cls: latency_summary(vals) for cls, vals in deltas_by_class.items()
        },
    }


def fmt_ms(ns) -> str:
    """Nanosecond delta -> milliseconds with microsecond precision."""
    if ns is None:
        return "-"
    return f"{ns / 1e6:+.3f}ms"


def fmt_ts_ms(ts_ms) -> str:
    """Exchange epoch ms (UTC) -> IST human string."""
    if ts_ms is None:
        return "-"
    dt = datetime.fromtimestamp(ts_ms / 1000.0, tz=IST)
    return dt.strftime("%Y-%m-%d %H:%M:%S.%f")[:-3] + " IST"


def render_report(result: dict) -> str:
    """Human-readable stdout report."""
    ps: SideStats = result["python_stats"]
    rs: SideStats = result["rust_stats"]
    lines = []
    a = lines.append
    a("=" * 76)
    a("GROWW PARITY + LATENCY REPORT  (PR-R2 — python sidecar vs native rust)")
    a("clock basis: BOTH capture_ns stamps come from the SAME host wall clock")
    a("  (python time.time_ns() / rust SystemTime); delta = python - rust;")
    a("  positive delta = python captured LATER (rust was faster).")
    a("join key: (segment, security_id, exchange_ts_millis) — exchange identity")
    a("  is folded into the canonical segment string (no exchange field exists).")
    a("=" * 76)
    for stats, exists in ((ps, result["python_exists"]), (rs, result["rust_exists"])):
        a(f"[{stats.name}] file present: {'yes' if exists else 'NO — side absent'}")
        a(
            f"  lines={stats.total_lines} parsed={stats.parsed} "
            f"malformed={stats.malformed} blank={stats.blank} "
            f"unjoinable_ts0={stats.unjoinable} dup_keys={stats.duplicates} "
            f"no_capture_ns={stats.no_capture_ns}"
        )
        a(
            f"  first_tick={fmt_ts_ms(stats.first_ts_ms)}  "
            f"last_tick={fmt_ts_ms(stats.last_ts_ms)}"
        )
        for s in stats.malformed_samples:
            a(f"  malformed sample: {s}")
    a("-" * 76)
    a(
        f"unique keys: python={result['python_unique']} rust={result['rust_unique']}"
        f"  matched={result['matched']}"
    )
    a(
        f"missing-in-rust={result['missing_in_rust']} "
        f"(index={result['missing_in_rust_by_class']['index']}, "
        f"stock={result['missing_in_rust_by_class']['stock']})"
    )
    a(
        f"missing-in-python={result['missing_in_python']} "
        f"(index={result['missing_in_python_by_class']['index']}, "
        f"stock={result['missing_in_python_by_class']['stock']})"
    )
    a(
        f"matched-without-latency (capture_ns absent on a side): "
        f"{result['matched_no_latency']}"
    )
    if result["ltp_mismatch"]:
        a(f"ltp mismatch on matched keys: {result['ltp_mismatch']}")
        for s in result["ltp_mismatch_samples"]:
            a(f"  ltp sample: {s}")
    a("-" * 76)
    a("receipt-latency delta (python_capture - rust_capture):")
    header = f"{'class':<8}{'count':>9}{'p50':>12}{'p90':>12}{'p99':>12}{'min':>12}{'max':>12}"
    a(header)
    rows = [("all", result["latency_all"])] + [
        (cls, result["latency_by_class"][cls]) for cls in ("index", "stock")
    ]
    for name, s in rows:
        if s["count"] == 0:
            a(f"{name:<8}{0:>9}" + "           -" * 5)
        else:
            a(
                f"{name:<8}{s['count']:>9}{fmt_ms(s['p50']):>12}"
                f"{fmt_ms(s['p90']):>12}{fmt_ms(s['p99']):>12}"
                f"{fmt_ms(s['min']):>12}{fmt_ms(s['max']):>12}"
            )
    a("=" * 76)
    return "\n".join(lines)


def render_telegram_summary(result: dict) -> str:
    """Telegram-ready 5-line plain-English summary (auto-driver rules)."""
    s = result["latency_all"]
    if s["count"]:
        speed = (
            f"Rust was faster on average by {abs(s['mean']) / 1e6:.1f}ms"
            if s["mean"] > 0
            else f"Python was faster on average by {abs(s['mean']) / 1e6:.1f}ms"
        )
        lat = f"{speed} (middle tick {fmt_ms(s['p50'])}, worst {fmt_ms(s['max'])})"
    else:
        lat = "No latency numbers yet (no tick had time stamps on both sides)"
    total_missing = result["missing_in_rust"] + result["missing_in_python"]
    verdict = "✅" if total_missing == 0 and result["matched"] > 0 else "⚠️"
    return "\n".join(
        [
            f"{verdict} Groww feed comparison: Python vs Rust",
            f"Ticks seen — Python: {result['python_unique']}, "
            f"Rust: {result['rust_unique']}, both: {result['matched']}",
            f"Rust missed {result['missing_in_rust']} ticks, "
            f"Python missed {result['missing_in_python']} ticks",
            lat,
            "Summary table saved to the parity file on disk",
        ]
    )


def write_tsv(result: dict, tsv_dir: str, ist_date: str) -> str:
    """Summary TSV to data/groww/parity-<IST-date>.tsv. Returns the path."""
    os.makedirs(tsv_dir, exist_ok=True)
    path = os.path.join(tsv_dir, f"parity-{ist_date}.tsv")
    ps: SideStats = result["python_stats"]
    rs: SideStats = result["rust_stats"]
    rows = [
        ("metric", "value"),
        ("python_total_lines", ps.total_lines),
        ("python_parsed", ps.parsed),
        ("python_malformed", ps.malformed),
        ("python_duplicates", ps.duplicates),
        ("python_unjoinable_ts0", ps.unjoinable),
        ("python_no_capture_ns", ps.no_capture_ns),
        ("python_unique_keys", result["python_unique"]),
        ("rust_total_lines", rs.total_lines),
        ("rust_parsed", rs.parsed),
        ("rust_malformed", rs.malformed),
        ("rust_duplicates", rs.duplicates),
        ("rust_unjoinable_ts0", rs.unjoinable),
        ("rust_unique_keys", result["rust_unique"]),
        ("matched", result["matched"]),
        ("matched_index", result["matched_by_class"]["index"]),
        ("matched_stock", result["matched_by_class"]["stock"]),
        ("matched_no_latency", result["matched_no_latency"]),
        ("missing_in_rust", result["missing_in_rust"]),
        ("missing_in_rust_index", result["missing_in_rust_by_class"]["index"]),
        ("missing_in_rust_stock", result["missing_in_rust_by_class"]["stock"]),
        ("missing_in_python", result["missing_in_python"]),
        ("missing_in_python_index", result["missing_in_python_by_class"]["index"]),
        ("missing_in_python_stock", result["missing_in_python_by_class"]["stock"]),
        ("ltp_mismatch", result["ltp_mismatch"]),
    ]
    for scope in ("all", "index", "stock"):
        s = (
            result["latency_all"]
            if scope == "all"
            else result["latency_by_class"][scope]
        )
        rows.append((f"latency_{scope}_count", s["count"]))
        for stat in ("p50", "p90", "p99", "min", "max", "mean"):
            rows.append(
                (
                    f"latency_{scope}_{stat}_ns",
                    int(s[stat]) if s["count"] else "",
                )
            )
    with open(path, "w", encoding="utf-8") as fh:
        for k, v in rows:
            fh.write(f"{k}\t{v}\n")
    return path


def ist_today() -> str:
    return datetime.now(tz=IST).strftime("%Y-%m-%d")


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        description="Groww python-vs-rust tick parity + latency comparer (PR-R2)"
    )
    ap.add_argument("--python", default=DEFAULT_PYTHON_PATH, dest="python_path")
    ap.add_argument("--rust", default=DEFAULT_RUST_PATH, dest="rust_path")
    ap.add_argument("--tsv-dir", default=DEFAULT_TSV_DIR)
    ap.add_argument("--no-tsv", action="store_true", help="skip the TSV output")
    args = ap.parse_args(argv)

    if not os.path.exists(args.python_path) and not os.path.exists(args.rust_path):
        print(
            f"ERROR: neither input exists: {args.python_path} / {args.rust_path}",
            file=sys.stderr,
        )
        return 2

    result = compare(args.python_path, args.rust_path)
    print(render_report(result))

    if not args.no_tsv:
        try:
            tsv_path = write_tsv(result, args.tsv_dir, ist_today())
            print(f"TSV written: {tsv_path}")
        except OSError as exc:
            print(f"ERROR: TSV write failed: {exc}", file=sys.stderr)
            print()
            print(render_telegram_summary(result))
            return 2

    print()
    print(render_telegram_summary(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
