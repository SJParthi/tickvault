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
    RAW_PROBE_HEARTBEAT_SECS,
    RAW_PROBE_INDEX_SAMPLE_COUNT,
    RAW_PROBE_SAMPLE_COUNT,
    RawTickProbe,
    WALK_INTERVAL_MS_DEFAULT,
    WALK_INTERVAL_MS_MAX,
    WALK_INTERVAL_MS_MIN,
    WATERMARK_MAX_LAG_MS_DEFAULT,
    dedup_should_emit,
    probe_field_nonzero,
    resolve_walk_interval_ms,
    resolve_watermark_max_lag_ms,
    ts_watermark_advanced,
    watermark_lag_should_fire,
    watermark_lag_stalled,
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


class ResolveWalkIntervalTests(unittest.TestCase):
    """The coalesced-walk interval clamp (2026-07-03 lag fix #1): the env
    override can tune the cadence but can never spin-loop the walker (0ms) or
    stall the feed behind a fat coalesce window (huge ms)."""

    def test_absent_env_resolves_to_default(self) -> None:
        self.assertEqual(resolve_walk_interval_ms(None), WALK_INTERVAL_MS_DEFAULT)

    def test_garbage_env_resolves_to_default(self) -> None:
        for bad in ("garbage", "", {}, []):
            self.assertEqual(resolve_walk_interval_ms(bad), WALK_INTERVAL_MS_DEFAULT)

    def test_zero_and_negative_clamp_to_min(self) -> None:
        self.assertEqual(resolve_walk_interval_ms("0"), WALK_INTERVAL_MS_MIN)
        self.assertEqual(resolve_walk_interval_ms("-500"), WALK_INTERVAL_MS_MIN)

    def test_huge_clamps_to_max(self) -> None:
        self.assertEqual(resolve_walk_interval_ms("999999"), WALK_INTERVAL_MS_MAX)

    def test_in_range_passes_through(self) -> None:
        self.assertEqual(resolve_walk_interval_ms("200"), 200)
        self.assertEqual(resolve_walk_interval_ms("50"), 50)

    def test_watermark_lag_env_clamp_shares_the_same_contract(self) -> None:
        self.assertEqual(
            resolve_watermark_max_lag_ms(None), WATERMARK_MAX_LAG_MS_DEFAULT
        )
        self.assertEqual(
            resolve_watermark_max_lag_ms("junk"), WATERMARK_MAX_LAG_MS_DEFAULT
        )


class WatermarkLagStalledTests(unittest.TestCase):
    """The watermark-lag stall verdict (2026-07-03 lag fix #2): micro-advances
    (+2 ms) must not hide a watermark drifting minutes behind wall-clock —
    but off-market lag and an unknown watermark must NEVER count as stalled."""

    NOW_MS = 1_783_069_320_000.0
    THR = WATERMARK_MAX_LAG_MS_DEFAULT  # 120_000

    def test_119s_lag_is_not_stalled(self) -> None:
        self.assertFalse(
            watermark_lag_stalled(self.NOW_MS, int(self.NOW_MS) - 119_000, True, self.THR)
        )

    def test_exactly_threshold_lag_is_not_stalled(self) -> None:
        # Strict > : exactly 120.000s is NOT past the threshold.
        self.assertFalse(
            watermark_lag_stalled(self.NOW_MS, int(self.NOW_MS) - 120_000, True, self.THR)
        )

    def test_121s_lag_is_stalled(self) -> None:
        self.assertTrue(
            watermark_lag_stalled(self.NOW_MS, int(self.NOW_MS) - 121_000, True, self.THR)
        )

    def test_off_market_lag_is_never_stalled(self) -> None:
        # Overnight the watermark legitimately sits at yesterday 15:29 IST.
        self.assertFalse(
            watermark_lag_stalled(self.NOW_MS, int(self.NOW_MS) - 999_000, False, self.THR)
        )

    def test_unknown_watermark_is_never_stalled(self) -> None:
        # Cold / never-streamed: the silent-feed diagnostic + the Rust
        # process-kill backstop own that case, never this criterion.
        self.assertFalse(watermark_lag_stalled(self.NOW_MS, 0, True, self.THR))
        self.assertFalse(watermark_lag_stalled(self.NOW_MS, -1, True, self.THR))

    def test_future_watermark_is_not_stalled(self) -> None:
        # A watermark slightly AHEAD of our clock (skew) is negative lag.
        self.assertFalse(
            watermark_lag_stalled(self.NOW_MS, int(self.NOW_MS) + 5_000, True, self.THR)
        )


class WatermarkLagShouldFireTests(unittest.TestCase):
    """The fire decision: N consecutive stalled verdicts + refire cooldown so
    a backlog that survives one reconnect refires at a bounded cadence."""

    def test_below_consecutive_does_not_fire(self) -> None:
        self.assertFalse(watermark_lag_should_fire(2, 3, 0.0))

    def test_at_consecutive_fires(self) -> None:
        self.assertTrue(watermark_lag_should_fire(3, 3, 0.0))

    def test_above_consecutive_fires(self) -> None:
        self.assertTrue(watermark_lag_should_fire(10, 3, 0.0))

    def test_cooldown_suppresses_refire(self) -> None:
        self.assertFalse(watermark_lag_should_fire(10, 3, 0.1))

    def test_expired_cooldown_fires_again(self) -> None:
        self.assertTrue(watermark_lag_should_fire(3, 3, 0.0))
        self.assertTrue(watermark_lag_should_fire(3, 3, -5.0))


def _stock_tick(i: int, volume: float = 0.0, oi: float = 0.0) -> dict:
    """A realistic 16-field-style decoded stock/FNO leaf dict (proto3 doubles
    always present — an unpopulated field is 0.0, exactly the ambiguity the
    probe exists to settle)."""
    return {
        "tsInMillis": 1_783_069_200_000 + i,
        "ltp": 100.0 + i,
        "volume": volume,
        "openInterest": oi,
        "open": 99.0,
        "high": 101.0,
        "low": 98.0,
        "close": 0.0,
        "avgPrice": 100.5,
        "bidQty": 10.0,
        "offerQty": 12.0,
    }


class ProbeFieldNonzeroTests(unittest.TestCase):
    """probe_field_nonzero: proto3 no-presence 0.0, an absent key (None) and
    garbage must all honestly read \"not populated\" — never raise."""

    def test_nonzero_numeric_true(self) -> None:
        self.assertTrue(probe_field_nonzero(5000.0))
        self.assertTrue(probe_field_nonzero(-1.0))
        self.assertTrue(probe_field_nonzero("3"))

    def test_zero_is_false(self) -> None:
        self.assertFalse(probe_field_nonzero(0.0))
        self.assertFalse(probe_field_nonzero(0))

    def test_garbage_never_raises_and_is_false(self) -> None:
        for bad in (None, "x", {}, [], ""):
            self.assertFalse(probe_field_nonzero(bad))


class RawTickProbeTests(unittest.TestCase):
    """The RAW-TICK-PROBE sampler (2026-07-03, log-only): bounded at N,
    summary counts correct, running counters are cheap process-lifetime ints,
    GROWW_RAW_PROBE=always re-arms sampling per reconnect cycle."""

    def setUp(self) -> None:
        self.lines = []
        self.probe = RawTickProbe(mode="", emit=self.lines.append)

    def _samples(self, kind: str) -> list:
        return [l for l in self.lines if l.startswith(f"RAW-TICK-PROBE kind={kind}")]

    def _summaries(self) -> list:
        return [l for l in self.lines if l.startswith("RAW-TICK-PROBE-SUMMARY")]

    def test_ltp_sampling_bounded_at_n(self) -> None:
        for i in range(50):
            self.probe.observe("ltp", str(2885 + i), _stock_tick(i))
        self.assertEqual(len(self._samples("ltp")), RAW_PROBE_SAMPLE_COUNT)

    def test_sample_line_carries_full_dict_as_json(self) -> None:
        self.probe.observe("ltp", "2885", _stock_tick(0, volume=5000.0))
        line = self._samples("ltp")[0]
        self.assertIn("token=2885", line)
        self.assertIn('"volume":5000.0', line)
        self.assertIn('"openInterest":0.0', line)
        self.assertIn('"ltp":100.0', line)

    def test_summary_fires_once_with_correct_counts(self) -> None:
        # 3 of the first N carry nonzero volume; 1 carries nonzero OI.
        for i in range(RAW_PROBE_SAMPLE_COUNT):
            self.probe.observe(
                "ltp",
                str(i),
                _stock_tick(i, volume=7.0 if i < 3 else 0.0, oi=9.0 if i == 0 else 0.0),
            )
        summaries = self._summaries()
        self.assertEqual(len(summaries), 1)
        n = RAW_PROBE_SAMPLE_COUNT
        self.assertIn(f"nonzero_volume_ticks=3/{n}", summaries[0])
        self.assertIn(f"nonzero_oi=1/{n}", summaries[0])
        # every _stock_tick has nonzero open/high/low -> all OHLC-populated.
        self.assertIn(f"nonzero_ohlc={n}/{n}", summaries[0])
        # More ticks NEVER re-fire the summary in default (once-per-process) mode.
        for i in range(10):
            self.probe.observe("ltp", str(100 + i), _stock_tick(i))
        self.assertEqual(len(self._summaries()), 1)

    def test_no_summary_before_sampling_completes(self) -> None:
        for i in range(RAW_PROBE_SAMPLE_COUNT - 1):
            self.probe.observe("ltp", str(i), _stock_tick(i))
        self.assertEqual(self._summaries(), [])

    def test_index_sampling_bounded_and_separate(self) -> None:
        for _ in range(10):
            self.probe.observe("index", "NIFTY", {"tsInMillis": 1, "value": 26965.05})
        self.assertEqual(len(self._samples("index")), RAW_PROBE_INDEX_SAMPLE_COUNT)
        # Index dicts never count toward the stock/FNO running counters.
        self.assertEqual(self.probe.total_ltp_ticks, 0)

    def test_running_counters_cover_every_drained_tick(self) -> None:
        # 100 ticks, volume nonzero on every 4th, OI nonzero on every 10th —
        # counters keep counting far past the 8-sample bound (plain int state).
        for i in range(100):
            self.probe.observe(
                "ltp",
                str(i),
                _stock_tick(i, volume=1.0 if i % 4 == 0 else 0.0,
                            oi=2.0 if i % 10 == 0 else 0.0),
            )
        self.assertEqual(self.probe.total_ltp_ticks, 100)
        self.assertEqual(self.probe.nonzero_volume_ticks, 25)
        self.assertEqual(self.probe.nonzero_oi_ticks, 10)
        self.assertIsInstance(self.probe.total_ltp_ticks, int)
        self.assertIsInstance(self.probe.nonzero_volume_ticks, int)
        self.assertIsInstance(self.probe.nonzero_oi_ticks, int)

    def test_non_dict_tick_ignored(self) -> None:
        for bad in (None, "x", 42, [1, 2]):
            self.probe.observe("ltp", "1", bad)
        self.assertEqual(self.probe.total_ltp_ticks, 0)
        self.assertEqual(self.lines, [])

    def test_default_mode_rearm_is_noop(self) -> None:
        for i in range(RAW_PROBE_SAMPLE_COUNT):
            self.probe.observe("ltp", str(i), _stock_tick(i))
        self.probe.rearm_for_reconnect()  # default mode: no-op
        self.probe.observe("ltp", "999", _stock_tick(999))
        self.assertEqual(len(self._samples("ltp")), RAW_PROBE_SAMPLE_COUNT)
        self.assertEqual(len(self._summaries()), 1)

    def test_always_mode_rearm_resamples_but_keeps_counters(self) -> None:
        lines = []
        probe = RawTickProbe(mode="always", ltp_limit=2, index_limit=1,
                             emit=lines.append)
        self.assertTrue(probe.always)
        for i in range(5):
            probe.observe("ltp", str(i), _stock_tick(i, volume=1.0))
        first_cycle = [l for l in lines if l.startswith("RAW-TICK-PROBE kind=ltp")]
        self.assertEqual(len(first_cycle), 2)
        probe.rearm_for_reconnect()  # the reconnect cycle re-arms sampling…
        for i in range(5):
            probe.observe("ltp", str(100 + i), _stock_tick(i))
        both_cycles = [l for l in lines if l.startswith("RAW-TICK-PROBE kind=ltp")]
        self.assertEqual(len(both_cycles), 4)
        self.assertEqual(
            len([l for l in lines if l.startswith("RAW-TICK-PROBE-SUMMARY")]), 2
        )
        # …but the process-lifetime running counters were NEVER reset.
        self.assertEqual(probe.total_ltp_ticks, 10)
        self.assertEqual(probe.nonzero_volume_ticks, 5)

    def test_env_mode_string_parsing(self) -> None:
        self.assertTrue(RawTickProbe(mode="always").always)
        self.assertTrue(RawTickProbe(mode=" ALWAYS ").always)
        self.assertFalse(RawTickProbe(mode="").always)
        self.assertFalse(RawTickProbe(mode="once").always)
        self.assertFalse(RawTickProbe(mode=None).always)

    def test_heartbeat_bounded_cadence(self) -> None:
        t0 = 1000.0
        # No ticks yet -> never fires.
        self.assertFalse(self.probe.maybe_heartbeat(t0))
        self.probe.observe("ltp", "1", _stock_tick(1, volume=3.0))
        # First call arms the cadence, no line yet.
        self.assertFalse(self.probe.maybe_heartbeat(t0))
        # Within the interval -> silent.
        self.assertFalse(self.probe.maybe_heartbeat(t0 + RAW_PROBE_HEARTBEAT_SECS - 1))
        # Past the interval -> exactly one line with the running counters.
        self.assertTrue(self.probe.maybe_heartbeat(t0 + RAW_PROBE_HEARTBEAT_SECS))
        beats = [l for l in self.lines if l.startswith("RAW-TICK-PROBE-HEARTBEAT")]
        self.assertEqual(len(beats), 1)
        self.assertIn("nonzero_volume_ticks=1/1", beats[0])
        self.assertIn("nonzero_oi=0/1", beats[0])
        # Immediately after firing -> silent again until the next interval.
        self.assertFalse(self.probe.maybe_heartbeat(t0 + RAW_PROBE_HEARTBEAT_SECS + 1))


if __name__ == "__main__":
    unittest.main()
