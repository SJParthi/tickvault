#!/usr/bin/env python3
"""Unit tests for the sidecar's pure change-dedup decision (2026-07-03 fix).

Runnable WITHOUT the growwapi SDK installed:
    cd scripts/groww-sidecar && python3 -m unittest test_dedup -v

A stub `growwapi` module is injected before importing groww_sidecar because the
sidecar hard-exits when the SDK import fails (prod behaviour we must not
weaken). Importing the module is side-effect-free beyond logging config; main()
is never called here.

NOT wired into CI (no Python test harness exists in CI today) — the same
assertions also run under `python3 groww_sidecar.py --selftest`
(_selftest_dedup), which the deployed environment can execute.
"""
import sys
import types
import unittest

# Stub the SDK so `from growwapi import GrowwAPI, GrowwFeed` succeeds without
# the wheel. Tests exercise ONLY the pure dedup helper — never the SDK surface.
if "growwapi" not in sys.modules:
    _stub = types.ModuleType("growwapi")
    _stub.GrowwAPI = object
    _stub.GrowwFeed = object
    sys.modules["growwapi"] = _stub

from groww_sidecar import (  # noqa: E402  (stub must precede)
    dedup_should_emit,
    ts_watermark_advanced,
)

TS0 = 1_783_069_200_183  # the live incident's frozen 09:00:00.183 IST print
PRICE0 = 26965.05


class DedupShouldEmitTests(unittest.TestCase):
    def setUp(self) -> None:
        self.cache = {}
        self.key = ("idx", "NSE", "CASH", "NIFTY")

    def test_first_emit_allowed(self) -> None:
        self.assertTrue(dedup_should_emit(self.cache, self.key, TS0, PRICE0))

    def test_identical_ts_and_price_skipped(self) -> None:
        dedup_should_emit(self.cache, self.key, TS0, PRICE0)
        # The 530K-row flood shape: the identical snapshot entry re-dumped on
        # every NATS callback must be skipped — every repeat, not just once.
        for _ in range(25):
            self.assertFalse(dedup_should_emit(self.cache, self.key, TS0, PRICE0))

    def test_advancing_ts_same_price_emitted(self) -> None:
        # Adversarial guard: a genuine new print with an IDENTICAL price but an
        # advancing tsInMillis must NEVER be swallowed (dedup key includes ts).
        dedup_should_emit(self.cache, self.key, TS0, PRICE0)
        self.assertTrue(dedup_should_emit(self.cache, self.key, TS0 + 1000, PRICE0))

    def test_same_ts_changed_price_emitted(self) -> None:
        dedup_should_emit(self.cache, self.key, TS0, PRICE0)
        self.assertTrue(dedup_should_emit(self.cache, self.key, TS0, PRICE0 + 0.05))

    def test_keys_are_independent(self) -> None:
        dedup_should_emit(self.cache, self.key, TS0, PRICE0)
        other = ("ltp", "NSE", "CASH", "2885")
        self.assertTrue(dedup_should_emit(self.cache, other, TS0, PRICE0))
        # …and each key still dedups its own repeats.
        self.assertFalse(dedup_should_emit(self.cache, other, TS0, PRICE0))

    def test_kind_discriminates_index_vs_stock(self) -> None:
        # BSE SENSEX subscribes with numeric token "1" (kind=idx); a stock with
        # the same (exchange, segment, token) tuple must not share dedup state.
        idx_key = ("idx", "BSE", "CASH", "1")
        stock_key = ("ltp", "BSE", "CASH", "1")
        self.assertTrue(dedup_should_emit(self.cache, idx_key, TS0, PRICE0))
        self.assertTrue(dedup_should_emit(self.cache, stock_key, TS0, PRICE0))

    def test_cache_bounded_by_distinct_keys(self) -> None:
        # Bounded memory: N distinct instruments -> exactly N entries no matter
        # how many re-dumps arrive (the live flood was ~525 callbacks/sec).
        keys = [("idx", "NSE", "CASH", f"IDX{i}") for i in range(25)]
        for _ in range(100):
            for i, key in enumerate(keys):
                dedup_should_emit(self.cache, key, TS0, PRICE0 + i)
        self.assertEqual(len(self.cache), 25)

    def test_older_ts_replay_emits_once_then_dedups(self) -> None:
        # Dedup is CHANGE-based (last-emitted pair), not monotonic: a snapshot
        # that steps back (SDK re-seed) emits once, then its repeats dedup.
        # Row-level idempotency of a restart burst is QuestDB DEDUP's job.
        dedup_should_emit(self.cache, self.key, TS0 + 5000, PRICE0)
        self.assertTrue(dedup_should_emit(self.cache, self.key, TS0, PRICE0))
        self.assertFalse(dedup_should_emit(self.cache, self.key, TS0, PRICE0))


class TsWatermarkAdvancedTests(unittest.TestCase):
    """The stall detector's liveness signal (2026-07-03 feed-death fix): only
    an ADVANCING max exchange timestamp counts as feed liveness — a frozen
    snapshot re-dump (the 09:07:55→09:38:53 incident, 1.16M stale decodes with
    zero fresh data) must read as stalled."""

    def test_first_ts_advances(self) -> None:
        self.assertTrue(ts_watermark_advanced(0, TS0))

    def test_frozen_ts_does_not_advance(self) -> None:
        self.assertFalse(ts_watermark_advanced(TS0, TS0))

    def test_older_replayed_ts_does_not_advance(self) -> None:
        self.assertFalse(ts_watermark_advanced(TS0, TS0 - 60_000))

    def test_float_payload_ts_advances(self) -> None:
        # Groww docs show tsInMillis as a float (e.g. 1746174479582.0).
        self.assertTrue(ts_watermark_advanced(TS0, float(TS0 + 1000)))

    def test_garbage_ts_never_raises_and_never_advances(self) -> None:
        for bad in (None, "not-a-ts", {}, []):
            self.assertFalse(ts_watermark_advanced(0, bad))


if __name__ == "__main__":
    unittest.main()
