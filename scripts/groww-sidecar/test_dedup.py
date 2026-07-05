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
import json
import os
import queue
import sys
import tempfile
import time
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
    CAPTURE_QUEUE_MAX,
    RAW_PROBE_HEARTBEAT_SECS,
    RAW_PROBE_INDEX_SAMPLE_COUNT,
    RAW_PROBE_SAMPLE_COUNT,
    RECONCILE_INTERVAL_MS_DEFAULT,
    RECONCILE_INTERVAL_MS_MAX,
    RECONCILE_INTERVAL_MS_MIN,
    RawTickProbe,
    WALK_INTERVAL_MS_DEFAULT,
    WALK_INTERVAL_MS_MAX,
    WALK_INTERVAL_MS_MIN,
    WATERMARK_MAX_LAG_MS_DEFAULT,
    build_capture_topic_map,
    dedup_should_emit,
    install_callback_capture,
    make_capture_hook,
    probe_field_nonzero,
    resolve_callback_capture_enabled,
    resolve_reconcile_interval_ms,
    resolve_walk_interval_ms,
    resolve_watermark_max_lag_ms,
    ts_watermark_advanced,
    watermark_lag_should_fire,
    watermark_lag_stalled,
)
import groww_sidecar  # noqa: E402  (module handle for capture-path tests)

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


class CaptureResolverTests(unittest.TestCase):
    """GROWW_CALLBACK_CAPTURE kill-switch + GROWW_RECONCILE_INTERVAL_MS clamp."""

    def test_capture_enabled_by_default(self) -> None:
        self.assertTrue(resolve_callback_capture_enabled(None))
        self.assertTrue(resolve_callback_capture_enabled(""))
        self.assertTrue(resolve_callback_capture_enabled("on"))
        self.assertTrue(resolve_callback_capture_enabled("garbage"))

    def test_kill_switch_values_case_insensitive(self) -> None:
        for raw in ("off", "OFF", " Off ", "0", "false", "FALSE", "no", "No"):
            self.assertFalse(resolve_callback_capture_enabled(raw), raw)

    def test_reconcile_interval_default_and_clamp(self) -> None:
        self.assertEqual(
            resolve_reconcile_interval_ms(None), RECONCILE_INTERVAL_MS_DEFAULT
        )
        self.assertEqual(
            resolve_reconcile_interval_ms("garbage"), RECONCILE_INTERVAL_MS_DEFAULT
        )
        self.assertEqual(resolve_reconcile_interval_ms("1"), RECONCILE_INTERVAL_MS_MIN)
        self.assertEqual(
            resolve_reconcile_interval_ms("9999999"), RECONCILE_INTERVAL_MS_MAX
        )
        self.assertEqual(resolve_reconcile_interval_ms("7000"), 7000)


class CaptureHookTests(unittest.TestCase):
    """The enqueue-only NATS-callback wrapper: non-blocking, bounded, always
    delegates (design study §Q1 c2; starvation guard for the #1344 class)."""

    def test_hook_delegates_and_enqueues_with_capture_ns(self) -> None:
        delegated = []
        q = queue.Queue(maxsize=8)
        enq, drop = [0], [0]
        hook = make_capture_hook(lambda s, d: delegated.append((s, d)), q, enq, drop)
        before = time.time_ns()
        hook("/ld/eq/nse/price.2885", b"\x01\x02")
        after = time.time_ns()
        self.assertEqual(delegated, [("/ld/eq/nse/price.2885", b"\x01\x02")])
        self.assertEqual((enq[0], drop[0]), (1, 0))
        subject, payload, capture_ns = q.get_nowait()
        self.assertEqual(subject, "/ld/eq/nse/price.2885")
        self.assertEqual(payload, b"\x01\x02")
        self.assertTrue(before <= capture_ns <= after, "capture_ns stamped in-hook")

    def test_hook_full_queue_drops_to_counter_never_blocks(self) -> None:
        q = queue.Queue(maxsize=2)
        enq, drop = [0], [0]
        delegated = [0]

        def orig(_s, _d):
            delegated[0] += 1

        hook = make_capture_hook(orig, q, enq, drop)
        start = time.monotonic()
        for i in range(10_000):
            hook(f"subj{i}", b"p")  # 9,998 hit a FULL queue
        elapsed = time.monotonic() - start
        self.assertEqual(enq[0], 2)
        self.assertEqual(drop[0], 9_998, "overflow drops to the counter")
        self.assertEqual(delegated[0], 10_000, "delegation NEVER skipped")
        self.assertEqual(q.qsize(), 2, "queue stays bounded")
        # Non-blocking proof: 10K full-queue calls complete in well under a
        # second (a blocking put would hang forever with no consumer).
        self.assertLess(elapsed, 1.0, f"hook must never block (took {elapsed:.3f}s)")

    def test_queue_bound_constant_is_sane(self) -> None:
        # ≈25 min of buffer at the measured ~42 msgs/sec steady rate.
        self.assertEqual(CAPTURE_QUEUE_MAX, 65536)

    def test_install_returns_false_without_sdk_internals(self) -> None:
        class NoNatsFeed:  # future-wheel shape: no _nats_client at all
            pass

        class NoCallbackFeed:
            class _NC:
                callback = None  # not callable

            _nats_client = _NC()

        self.assertFalse(install_callback_capture(NoNatsFeed(), queue.Queue()))
        self.assertFalse(install_callback_capture(NoCallbackFeed(), queue.Queue()))

    def test_install_wraps_callback_and_preserves_delegation(self) -> None:
        seen = []

        class NC:
            def callback(self, subject, data):  # bound method — callable
                seen.append((subject, data))

        class Feed:
            _nats_client = NC()

        feed = Feed()
        # Bind the ORIGINAL bound method the way the SDK does (instance attr).
        feed._nats_client.callback = feed._nats_client.callback
        q = queue.Queue(maxsize=4)
        self.assertTrue(install_callback_capture(feed, q))
        feed._nats_client.callback("/ld/indices/nse/price.NIFTY", b"raw")
        self.assertEqual(seen, [("/ld/indices/nse/price.NIFTY", b"raw")])
        self.assertEqual(q.qsize(), 1)

    def test_topic_map_build_degrades_to_none_without_sdk(self) -> None:
        # The stubbed growwapi has no .groww.constants — the builder must
        # return None (walker-only fallback), never raise (c1-fallback rule).
        result = build_capture_topic_map(
            [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}],
            [],
            {("NSE", "CASH", "2885"): 2885},
        )
        self.assertIsNone(result)


class _FakeTopic:
    def __init__(self, topic, meta):
        self._topic = topic
        self._meta = meta

    def get_topic(self):
        return self._topic

    def get_meta(self):
        return self._meta


class _FakeFeedConstants:
    """Mirrors growwapi==1.5.0 FeedConstants topic-builder surface (verified
    against the wheel source: subjects /ld/{eq,fo}/{nse,bse}/price.<token> and
    /ld/indices/{nse,bse}/price.<token>; index meta segment is ALWAYS CASH)."""

    EXCHANGE = "exchange"
    SEGMENT = "segment"
    FEED_KEY = "feed_key"
    FEED_TYPE = "feed_type"
    LIVE_DATA = "ltp"
    LIVE_INDEX = "index_value"

    @staticmethod
    def get_live_price_topic(segment, exchange, token):
        seg_path = "eq" if segment == "CASH" else "fo"
        return _FakeTopic(
            f"/ld/{seg_path}/{exchange.lower()}/price.{token}",
            {
                "exchange": exchange,
                "segment": segment,
                "feed_key": token,
                "feed_type": "ltp",
            },
        )

    @staticmethod
    def get_live_index_topic(segment, exchange, token):
        return _FakeTopic(
            f"/ld/indices/{exchange.lower()}/price.{token}",
            {
                "exchange": exchange,
                # The REAL SDK stamps index meta segment as CASH regardless of
                # the subscribe entry's segment — the map must follow the META.
                "segment": "CASH",
                "feed_key": token,
                "feed_type": "index_value",
            },
        )


def _install_fake_sdk_constants():
    """Register growwapi.groww.constants with the fake FeedConstants so the
    topic-map builder + decode-path tests exercise the real code paths."""
    groww_pkg = types.ModuleType("growwapi.groww")
    constants_mod = types.ModuleType("growwapi.groww.constants")
    constants_mod.FeedConstants = _FakeFeedConstants
    sys.modules["growwapi.groww"] = groww_pkg
    sys.modules["growwapi.groww.constants"] = constants_mod


class CaptureTopicMapTests(unittest.TestCase):
    def setUp(self) -> None:
        _install_fake_sdk_constants()

    def tearDown(self) -> None:
        sys.modules.pop("growwapi.groww.constants", None)
        sys.modules.pop("growwapi.groww", None)

    def test_topic_map_stock_and_index_entries(self) -> None:
        sid_map = {
            ("NSE", "CASH", "2885"): 2885,
            ("NSE", "CASH", "NIFTY"): 4611686018427387913,
        }
        topic_map = build_capture_topic_map(
            [{"exchange": "NSE", "segment": "CASH", "exchange_token": "2885"}],
            [{"exchange": "NSE", "segment": "CASH", "exchange_token": "NIFTY"}],
            sid_map,
        )
        self.assertEqual(
            topic_map["/ld/eq/nse/price.2885"],
            ("ltp", "NSE", "CASH", "2885", 2885, "NSE_EQ"),
        )
        self.assertEqual(
            topic_map["/ld/indices/nse/price.NIFTY"],
            ("idx", "NSE", "CASH", "NIFTY", 4611686018427387913, "IDX_I"),
        )

    def test_stock_falls_back_to_numeric_token_and_index_never_does(self) -> None:
        topic_map = build_capture_topic_map(
            [{"exchange": "NSE", "segment": "FNO", "exchange_token": "49081"}],
            [{"exchange": "NSE", "segment": "CASH", "exchange_token": "BANKNIFTY"}],
            {},  # empty sid_map
        )
        kind, _ex, _seg, _tok, sid, canonical = topic_map["/ld/fo/nse/price.49081"]
        self.assertEqual((kind, sid, canonical), ("ltp", 49081, "NSE_FNO"))
        _k, _e, _s, _t, idx_sid, _c = topic_map["/ld/indices/nse/price.BANKNIFTY"]
        self.assertEqual(idx_sid, 0, "index token name must NEVER coerce to an id")


class CaptureEmitTests(unittest.TestCase):
    """End-to-end writer-thread emit: capture_ns lands on the NDJSON row; the
    walker's reconcile sweep dedups against hook emissions (shared cache)."""

    def setUp(self) -> None:
        self._tmp = tempfile.NamedTemporaryFile(  # noqa: SIM115 - handle kept
            mode="a", suffix=".ndjson", delete=False
        )
        # Snapshot + isolate the module state the emit paths mutate.
        self._saved_last_emitted = dict(groww_sidecar._LAST_EMITTED)
        groww_sidecar._LAST_EMITTED.clear()
        self._saved_topic_map = groww_sidecar._CAPTURE_TOPIC_MAP[0]
        self._saved_decode = groww_sidecar._decode_captured_payload
        self._saved_write_status = groww_sidecar.write_status
        groww_sidecar.write_status = lambda *a, **k: None  # no status file I/O
        self._saved_deduped = groww_sidecar.DEDUPED_TOTAL

    def tearDown(self) -> None:
        groww_sidecar._LAST_EMITTED.clear()
        groww_sidecar._LAST_EMITTED.update(self._saved_last_emitted)
        groww_sidecar._CAPTURE_TOPIC_MAP[0] = self._saved_topic_map
        groww_sidecar._decode_captured_payload = self._saved_decode
        groww_sidecar.write_status = self._saved_write_status
        self._tmp.close()
        os.unlink(self._tmp.name)

    def _lines(self):
        self._tmp.flush()
        with open(self._tmp.name) as fh:
            return [json.loads(l) for l in fh.read().splitlines() if l]

    def test_capture_emit_writes_capture_ns_row(self) -> None:
        groww_sidecar._CAPTURE_TOPIC_MAP[0] = {
            "/ld/eq/nse/price.2885": ("ltp", "NSE", "CASH", "2885", 2885, "NSE_EQ"),
        }
        groww_sidecar._decode_captured_payload = lambda kind, payload: {
            "tsInMillis": TS0,
            "ltp": 2847.55,
            "volume": 0.0,
        }
        wrote = groww_sidecar._capture_emit_one(
            self._tmp, "/ld/eq/nse/price.2885", b"raw", 1_751_500_000_123_456_789
        )
        self.assertEqual(wrote, 1)
        rows = self._lines()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["security_id"], 2885)
        self.assertEqual(rows[0]["segment"], "NSE_EQ")
        self.assertEqual(rows[0]["exchange_ts_millis"], TS0)
        self.assertEqual(rows[0]["ltp"], 2847.55)
        self.assertEqual(rows[0]["capture_ns"], 1_751_500_000_123_456_789)

    def test_walker_reconcile_dedups_against_hook_emission(self) -> None:
        # 1. Hook path emits (ts, price) for stock 2885.
        groww_sidecar._CAPTURE_TOPIC_MAP[0] = {
            "/ld/eq/nse/price.2885": ("ltp", "NSE", "CASH", "2885", 2885, "NSE_EQ"),
        }
        groww_sidecar._decode_captured_payload = lambda kind, payload: {
            "tsInMillis": TS0,
            "ltp": 2847.55,
        }
        self.assertEqual(
            groww_sidecar._capture_emit_one(
                self._tmp, "/ld/eq/nse/price.2885", b"raw", 123
            ),
            1,
        )
        deduped_before = groww_sidecar.DEDUPED_TOTAL
        # 2. The reconcile sweep re-walks the SAME snapshot entry — the shared
        #    dedup cache must swallow it (no duplicate row).
        groww_sidecar.emit_ltp_records(
            self._tmp,
            {"NSE": {"CASH": {"2885": {"tsInMillis": TS0, "ltp": 2847.55}}}},
            {("NSE", "CASH", "2885"): 2885},
            reconcile=True,
        )
        self.assertEqual(len(self._lines()), 1, "reconcile must not double-emit")
        self.assertEqual(groww_sidecar.DEDUPED_TOTAL, deduped_before + 1)
        # 3. A GENUINELY new print (fresh ts) still emits from the sweep and
        #    counts as a hook-missed reconcile emission.
        reconcile_before = groww_sidecar.RECONCILE_EMITTED_TOTAL
        groww_sidecar.emit_ltp_records(
            self._tmp,
            {"NSE": {"CASH": {"2885": {"tsInMillis": TS0 + 1000, "ltp": 2848.0}}}},
            {("NSE", "CASH", "2885"): 2885},
            reconcile=True,
        )
        self.assertEqual(len(self._lines()), 2)
        self.assertEqual(groww_sidecar.RECONCILE_EMITTED_TOTAL, reconcile_before + 1)
        # The reconcile row is snapshot-sourced: NO capture_ns (Rust bridge
        # falls back to its per-wake receipt stamp — old-format compatible).
        self.assertNotIn("capture_ns", self._lines()[1])

    def test_hook_then_reconcile_then_hook_roundtrip_dedup(self) -> None:
        # Reverse order: walker emits first, hook re-delivery of the SAME
        # (ts, price) is swallowed by the shared cache.
        groww_sidecar.emit_ltp_records(
            self._tmp,
            {"NSE": {"CASH": {"2885": {"tsInMillis": TS0, "ltp": 100.0}}}},
            {("NSE", "CASH", "2885"): 2885},
            reconcile=False,
        )
        groww_sidecar._CAPTURE_TOPIC_MAP[0] = {
            "/ld/eq/nse/price.2885": ("ltp", "NSE", "CASH", "2885", 2885, "NSE_EQ"),
        }
        groww_sidecar._decode_captured_payload = lambda kind, payload: {
            "tsInMillis": TS0,
            "ltp": 100.0,
        }
        self.assertEqual(
            groww_sidecar._capture_emit_one(
                self._tmp, "/ld/eq/nse/price.2885", b"raw", 123
            ),
            0,
            "hook re-delivery of a walker-emitted (ts, price) must dedup",
        )
        self.assertEqual(len(self._lines()), 1)

    def test_subject_miss_and_decode_error_drop_not_raise(self) -> None:
        groww_sidecar._CAPTURE_TOPIC_MAP[0] = {}
        self.assertEqual(
            groww_sidecar._capture_emit_one(self._tmp, "/unknown", b"raw", 1), 0
        )
        groww_sidecar._CAPTURE_TOPIC_MAP[0] = {
            "/ld/eq/nse/price.1": ("ltp", "NSE", "CASH", "1", 1, "NSE_EQ"),
        }

        def boom(kind, payload):
            raise ValueError("bad proto")

        groww_sidecar._decode_captured_payload = boom
        self.assertEqual(
            groww_sidecar._capture_emit_one(self._tmp, "/ld/eq/nse/price.1", b"x", 1),
            0,
        )
        self.assertEqual(len(self._lines()), 0)
        self.assertGreaterEqual(
            groww_sidecar.DROP_REASONS.get("capture_subject_miss", 0), 1
        )
        self.assertGreaterEqual(
            groww_sidecar.DROP_REASONS.get("capture_decode_error", 0), 1
        )

    def test_write_record_omits_capture_ns_when_absent(self) -> None:
        groww_sidecar._write_record(self._tmp, 13, "IDX_I", TS0, 26965.05)
        groww_sidecar._write_record(
            self._tmp, 13, "IDX_I", TS0 + 1, 26966.0, capture_ns=42, sync=False
        )
        rows = self._lines()
        self.assertNotIn("capture_ns", rows[0], "legacy schema unchanged")
        self.assertEqual(rows[1]["capture_ns"], 42)


class NatsErrorFilterTests(unittest.TestCase):
    """FIX 19 item 4: the SDK's empty 'Error:' logger lines are enriched with
    the exception class/repr and coalesced (1st + every 30th) so Monday's log
    names WHY instead of printing a blank line every ~4.1s."""

    def setUp(self) -> None:
        import io
        import logging

        self._logger = logging.getLogger(groww_sidecar.SDK_NATS_LOGGER_NAME)
        self._logger.propagate = False
        self._buf = io.StringIO()
        self._handler = logging.StreamHandler(self._buf)
        self._logger.addHandler(self._handler)
        self._logger.setLevel(logging.DEBUG)
        # fresh filter per test (remove any left by another test)
        for f in list(self._logger.filters):
            if isinstance(f, groww_sidecar.NatsErrorDetailFilter):
                self._logger.removeFilter(f)
        groww_sidecar.install_sdk_error_filter([])

    def tearDown(self) -> None:
        self._logger.removeHandler(self._handler)
        for f in list(self._logger.filters):
            if isinstance(f, groww_sidecar.NatsErrorDetailFilter):
                self._logger.removeFilter(f)

    def _lines(self):
        return [ln for ln in self._buf.getvalue().splitlines() if ln]

    def test_empty_error_is_enriched_with_class_and_repr(self) -> None:
        class EmptyErr(Exception):
            def __str__(self) -> str:
                return ""

        self._logger.error("Error: %s", EmptyErr())
        lines = self._lines()
        self.assertEqual(len(lines), 1)
        self.assertIn("class=EmptyErr", lines[0])

    def test_repeats_coalesced_first_plus_every_30th(self) -> None:
        class EmptyErr(Exception):
            def __str__(self) -> str:
                return ""

        for _ in range(65):
            self._logger.error("Error: %s", EmptyErr())
        lines = self._lines()
        self.assertEqual(len(lines), 3, lines)  # 1st, 30th, 60th
        self.assertIn("seen 30x", lines[1])
        self.assertIn("seen 60x", lines[2])

    def test_non_error_records_pass_untouched(self) -> None:
        self._logger.error("connected to %s", "server")
        self.assertEqual(self._lines(), ["connected to server"])

    def test_non_empty_error_detail_not_enriched(self) -> None:
        self._logger.error("Error: %s", ValueError("real reason"))
        self.assertEqual(self._lines(), ["Error: real reason"])

    def test_install_is_idempotent(self) -> None:
        groww_sidecar.install_sdk_error_filter([])
        groww_sidecar.install_sdk_error_filter([])
        count = sum(
            isinstance(f, groww_sidecar.NatsErrorDetailFilter)
            for f in self._logger.filters
        )
        self.assertEqual(count, 1)


class RealPathNatsErrorTests(unittest.TestCase):
    """FIX 20: reproduce the REAL emission path the earlier unit tests missed.

    In production the SDK-named logger ("growwapi.groww.nats_client") has NO
    handler of its own — records PROPAGATE to the root stderr handler that
    basicConfig installed. The pre-FIX-20 tests attached a handler directly
    to the SDK logger with propagate=False, so they passed while the live
    run streamed bare "Error: " lines every ~4.1s (attempt 2, 2026-07-05
    13:21 IST — the filter was never attached because its install site
    lived in the post-subscribe success path). These tests install through
    the real two-layer install_sdk_error_filter and assert THROUGH
    propagation to a root handler.
    """

    def setUp(self) -> None:
        import io
        import logging

        self._io = io
        self._logging = logging
        self._logger = logging.getLogger(groww_sidecar.SDK_NATS_LOGGER_NAME)
        self._logger.propagate = True
        self._logger.setLevel(logging.DEBUG)
        for f in list(self._logger.filters):
            if isinstance(f, groww_sidecar.NatsErrorDetailFilter):
                self._logger.removeFilter(f)
        self._root = logging.getLogger()
        # sweep stale filter instances off ALL root handlers (fresh counts)
        for h in self._root.handlers:
            for f in list(h.filters):
                if isinstance(f, groww_sidecar.NatsErrorDetailFilter):
                    h.removeFilter(f)
        self._buf = io.StringIO()
        self._root_handler = logging.StreamHandler(self._buf)
        self._saved_level = self._root.level
        self._root.addHandler(self._root_handler)
        self._root.setLevel(logging.DEBUG)
        groww_sidecar.install_sdk_error_filter([])

    def tearDown(self) -> None:
        self._root.removeHandler(self._root_handler)
        self._root.setLevel(self._saved_level)
        for f in list(self._logger.filters):
            if isinstance(f, groww_sidecar.NatsErrorDetailFilter):
                self._logger.removeFilter(f)
        for h in self._root.handlers:
            for f in list(h.filters):
                if isinstance(f, groww_sidecar.NatsErrorDetailFilter):
                    h.removeFilter(f)
        self._logger.propagate = False

    def _lines(self):
        return [ln for ln in self._buf.getvalue().splitlines() if ln]

    def test_real_path_empty_exception_enriched_via_propagation(self) -> None:
        class EmptyStrException(Exception):
            def __str__(self) -> str:
                return ""

        self._logger.error("Error: %s", EmptyStrException())
        lines = self._lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertIn("class=EmptyStrException", lines[0])
        self.assertIn("EmptyStrException()", lines[0])

    def test_real_path_bare_error_no_args_names_the_gap(self) -> None:
        # The SDK pre-formatting the line ("Error: " with NO args) must
        # still never emit a bare line — the gap itself is named.
        self._logger.error("Error: ")
        lines = self._lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertIn("no reason from SDK — args empty", lines[0])

    def test_real_path_empty_string_arg_is_named(self) -> None:
        self._logger.error("Error: %s", "")
        lines = self._lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertIn("empty reason string from SDK", lines[0])

    def test_real_path_two_layers_never_double_count(self) -> None:
        # logger-level AND root-handler attachments see the SAME record; the
        # per-record marker must count it ONCE. With double counting, 16
        # emissions reach count 30 (printed) at emission 15 -> 2 lines.
        class EmptyStrException(Exception):
            def __str__(self) -> str:
                return ""

        for _ in range(16):
            self._logger.error("Error: %s", EmptyStrException())
        self.assertEqual(len(self._lines()), 1, self._lines())

    def test_real_path_other_sdk_logger_covered_by_handler_layer(self) -> None:
        # A record from a DIFFERENT SDK logger (no logger-level filter) is
        # still enriched by the root-handler layer.
        class EmptyStrException(Exception):
            def __str__(self) -> str:
                return ""

        other = self._logging.getLogger("nats.aio.client")
        saved = other.propagate
        other.propagate = True
        other.setLevel(self._logging.DEBUG)
        try:
            other.error("Error: %s", EmptyStrException())
        finally:
            other.propagate = saved
        lines = self._lines()
        self.assertEqual(len(lines), 1, lines)
        self.assertIn("class=EmptyStrException", lines[0])

    def test_real_path_repeats_coalesce_through_propagation(self) -> None:
        for _ in range(31):
            self._logger.error("Error: ")
        lines = self._lines()
        self.assertEqual(len(lines), 2, lines)  # 1st + 30th
        self.assertIn("seen 30x", lines[1])

    def test_fix20_install_sites_wired_at_process_start(self) -> None:
        # Source-scan ratchet: main() must install the filter + the class
        # wrap right after the redaction filter — the FIX 20 root cause was
        # the install living only in the post-subscribe success path.
        import inspect as _inspect

        src = _inspect.getsource(groww_sidecar.main)
        redaction = src.index("_install_sdk_log_redaction(")
        flt = src.index("install_sdk_error_filter(")
        wrap = src.index("install_sdk_error_cb_class_wrap(")
        self.assertLess(redaction, flt, "filter must install right after redaction")
        self.assertLess(flt, wrap, "class wrap must install at process start too")


if __name__ == "__main__":
    unittest.main()
