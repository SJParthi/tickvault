"""Unit tests for the depth-200 Python sidecar bridge.

Pure-function coverage for the parser and the ILP line builder.
Network paths (websocket connect, ILP socket send) are NOT exercised
here — those need an integration harness with a live QuestDB and a
mock Dhan WebSocket server.

Run with: python3 -m unittest scripts.test_depth_200_bridge
"""

from __future__ import annotations

import struct
import sys
import unittest
from pathlib import Path

# Make sibling import work regardless of CWD.
sys.path.insert(0, str(Path(__file__).resolve().parent))

import depth_200_bridge as bridge  # noqa: E402


class SubscriptionParseTests(unittest.TestCase):
    def test_valid_nse_fno(self) -> None:
        sub = bridge.Subscription.parse("72265:NSE_FNO")
        self.assertEqual(sub.security_id, 72265)
        self.assertEqual(sub.segment, "NSE_FNO")

    def test_valid_nse_eq(self) -> None:
        sub = bridge.Subscription.parse("1333:NSE_EQ")
        self.assertEqual(sub.security_id, 1333)
        self.assertEqual(sub.segment, "NSE_EQ")

    def test_missing_colon_rejected(self) -> None:
        with self.assertRaises(ValueError):
            bridge.Subscription.parse("72265")

    def test_invalid_segment_rejected(self) -> None:
        with self.assertRaises(ValueError):
            bridge.Subscription.parse("72265:BSE_EQ")  # not in VALID_SEGMENTS

    def test_non_numeric_sid_rejected(self) -> None:
        with self.assertRaises(ValueError):
            bridge.Subscription.parse("abc:NSE_FNO")


class FrameParserTests(unittest.TestCase):
    def _build_frame(
        self,
        resp_code: int,
        seg: int,
        sid: int,
        levels: list[tuple[float, int, int]],
    ) -> bytes:
        msg_len = bridge.HEADER_BYTES + len(levels) * bridge.LEVEL_BYTES
        header = struct.pack("<hBBiI", msg_len, resp_code, seg, sid, len(levels))
        body = b"".join(
            struct.pack("<dII", price, qty, orders)
            for price, qty, orders in levels
        )
        return header + body

    def test_parses_bid_frame_into_levels(self) -> None:
        frame = self._build_frame(
            bridge.BID_CODE,
            seg=2,
            sid=72265,
            levels=[(24050.5, 100, 5), (24050.0, 200, 8)],
        )
        result = bridge.parse_depth_frame(frame)
        self.assertIsNotNone(result)
        side, levels, sid, row_count, seg_str = result
        self.assertEqual(side, "BID")
        self.assertEqual(sid, 72265)
        self.assertEqual(seg_str, "NSE_FNO")
        self.assertEqual(row_count, 2)
        self.assertEqual(levels[0], (24050.5, 100, 5))
        self.assertEqual(levels[1], (24050.0, 200, 8))

    def test_parses_ask_frame(self) -> None:
        frame = self._build_frame(
            bridge.ASK_CODE,
            seg=2,
            sid=72266,
            levels=[(24051.0, 50, 3)],
        )
        result = bridge.parse_depth_frame(frame)
        self.assertIsNotNone(result)
        side, levels, sid, _row_count, _seg = result
        self.assertEqual(side, "ASK")
        self.assertEqual(sid, 72266)
        self.assertEqual(levels[0], (24051.0, 50, 3))

    def test_disconnect_frame_returns_none(self) -> None:
        frame = struct.pack(
            "<hBBiIH", 14, bridge.DISCONNECT_CODE, 2, 72265, 0, 805
        )
        self.assertIsNone(bridge.parse_depth_frame(frame))

    def test_unknown_response_code_returns_none(self) -> None:
        frame = struct.pack("<hBBiI", 12, 99, 2, 72265, 0)
        self.assertIsNone(bridge.parse_depth_frame(frame))

    def test_short_frame_returns_none(self) -> None:
        # Less than HEADER_BYTES => can't even parse header.
        self.assertIsNone(bridge.parse_depth_frame(b"\x00\x01\x02"))

    def test_zero_priced_levels_are_skipped(self) -> None:
        # Dhan pads remaining levels with zeros — we must not emit
        # ILP rows for those (would corrupt order-book reconstruction).
        frame = self._build_frame(
            bridge.BID_CODE,
            seg=2,
            sid=72265,
            levels=[(24050.5, 100, 5), (0.0, 0, 0)],
        )
        result = bridge.parse_depth_frame(frame)
        self.assertIsNotNone(result)
        _side, levels, _sid, _rc, _seg = result
        self.assertEqual(len(levels), 1, "zero-price padding must be dropped")

    def test_segment_mapping_nse_eq(self) -> None:
        frame = self._build_frame(
            bridge.BID_CODE,
            seg=1,
            sid=1333,
            levels=[(2500.0, 10, 1)],
        )
        result = bridge.parse_depth_frame(frame)
        self.assertIsNotNone(result)
        _side, _levels, _sid, _rc, seg_str = result
        self.assertEqual(seg_str, "NSE_EQ")

    def test_segment_mapping_unknown_byte(self) -> None:
        frame = self._build_frame(
            bridge.BID_CODE,
            seg=99,
            sid=42,
            levels=[(100.0, 1, 1)],
        )
        result = bridge.parse_depth_frame(frame)
        self.assertIsNotNone(result)
        _side, _levels, _sid, _rc, seg_str = result
        # Unknown bytes are surfaced verbatim so QuestDB has the raw
        # value rather than silently mapping it to one of our two known
        # segments. Operator can then triage.
        self.assertTrue(seg_str.startswith("UNKNOWN_"), seg_str)


class IlpLineBuilderTests(unittest.TestCase):
    def test_one_level_emits_one_line(self) -> None:
        payload = bridge.build_ilp_lines(
            table="deep_market_depth",
            segment="NSE_FNO",
            side="BID",
            security_id=72265,
            levels=[(24050.5, 100, 5)],
            received_nanos=1_777_380_615_832_000_000,
        )
        text = payload.decode("utf-8")
        self.assertEqual(text.count("\n"), 1)
        self.assertTrue(text.endswith("\n"))

    def test_three_levels_emit_three_lines(self) -> None:
        payload = bridge.build_ilp_lines(
            table="deep_market_depth",
            segment="NSE_FNO",
            side="ASK",
            security_id=67522,
            levels=[(55700.0, 50, 3), (55700.5, 75, 4), (55701.0, 100, 5)],
            received_nanos=1_777_380_615_832_000_000,
        )
        self.assertEqual(payload.count(b"\n"), 3)

    def test_ilp_line_format_matches_questdb_schema(self) -> None:
        payload = bridge.build_ilp_lines(
            table="deep_market_depth",
            segment="NSE_FNO",
            side="BID",
            security_id=72265,
            levels=[(24050.5, 100, 5)],
            received_nanos=1_777_380_615_832_000_000,
        )
        line = payload.decode("utf-8")
        # Symbols (segment, side, depth_type) must come first per QuestDB ILP spec.
        self.assertIn("deep_market_depth,segment=NSE_FNO,side=BID,depth_type=200 ", line)
        # Integer columns must use the `i` suffix.
        self.assertIn("security_id=72265i", line)
        self.assertIn("level=1i", line)
        self.assertIn("quantity=100i", line)
        self.assertIn("orders=5i", line)
        # Float column has no suffix.
        self.assertIn("price=24050.5,", line)
        # received_at uses `t` (microseconds) suffix.
        self.assertIn("received_at=1777380615832000t ", line)
        # Designated timestamp at end is nanoseconds, no suffix.
        self.assertTrue(line.rstrip().endswith(" 1777380615832000000"), line)

    def test_levels_are_one_indexed(self) -> None:
        payload = bridge.build_ilp_lines(
            table="deep_market_depth",
            segment="NSE_FNO",
            side="BID",
            security_id=1,
            levels=[(1.0, 1, 1), (2.0, 2, 2), (3.0, 3, 3)],
            received_nanos=1,
        )
        text = payload.decode("utf-8")
        self.assertIn("level=1i,", text)
        self.assertIn("level=2i,", text)
        self.assertIn("level=3i,", text)
        self.assertNotIn("level=0i,", text)

    def test_empty_levels_produce_empty_payload(self) -> None:
        payload = bridge.build_ilp_lines(
            table="deep_market_depth",
            segment="NSE_FNO",
            side="BID",
            security_id=72265,
            levels=[],
            received_nanos=1,
        )
        self.assertEqual(payload, b"")


class BridgeStateTests(unittest.TestCase):
    def test_parses_valid_state_dict(self) -> None:
        raw = {
            "client_id": "1106656882",
            "access_token": "eyJ...",
            "subscriptions": [
                {"security_id": 72265, "segment": "NSE_FNO"},
                {"security_id": 72266, "segment": "NSE_FNO"},
            ],
            "version": 7,
        }
        state = bridge.BridgeState.from_dict(raw)
        self.assertEqual(state.client_id, "1106656882")
        self.assertEqual(state.access_token, "eyJ...")
        self.assertEqual(state.version, 7)
        self.assertEqual(len(state.subscriptions), 2)
        self.assertEqual(state.subscriptions[0].security_id, 72265)

    def test_missing_credentials_rejected(self) -> None:
        with self.assertRaises(ValueError):
            bridge.BridgeState.from_dict(
                {"access_token": "x", "subscriptions": [], "version": 0}
            )
        with self.assertRaises(ValueError):
            bridge.BridgeState.from_dict(
                {"client_id": "x", "subscriptions": [], "version": 0}
            )

    def test_subscriptions_must_be_list(self) -> None:
        with self.assertRaises(ValueError):
            bridge.BridgeState.from_dict(
                {
                    "client_id": "x",
                    "access_token": "y",
                    "subscriptions": "not a list",
                    "version": 0,
                }
            )

    def test_invalid_segment_in_state_rejected(self) -> None:
        with self.assertRaises(ValueError):
            bridge.BridgeState.from_dict(
                {
                    "client_id": "x",
                    "access_token": "y",
                    "subscriptions": [
                        {"security_id": 1, "segment": "BSE_EQ"}  # not allowed
                    ],
                    "version": 0,
                }
            )

    def test_subscriptions_are_hashable_for_set_diff(self) -> None:
        # diff_subscriptions relies on Subscription being hashable (frozen dataclass).
        sub_a = bridge.Subscription(security_id=1, segment="NSE_FNO")
        sub_b = bridge.Subscription(security_id=2, segment="NSE_FNO")
        # Hashable + equality check.
        self.assertEqual(hash(sub_a), hash(bridge.Subscription(1, "NSE_FNO")))
        self.assertNotEqual(sub_a, sub_b)


class DiffSubscriptionsTests(unittest.TestCase):
    def _sub(self, sid: int) -> bridge.Subscription:
        return bridge.Subscription(security_id=sid, segment="NSE_FNO")

    def test_no_change_returns_empty_sets(self) -> None:
        old = (self._sub(1), self._sub(2))
        new = (self._sub(1), self._sub(2))
        to_remove, to_add = bridge.diff_subscriptions(old, new)
        self.assertEqual(to_remove, set())
        self.assertEqual(to_add, set())

    def test_pure_add(self) -> None:
        old = (self._sub(1),)
        new = (self._sub(1), self._sub(2), self._sub(3))
        to_remove, to_add = bridge.diff_subscriptions(old, new)
        self.assertEqual(to_remove, set())
        self.assertEqual(to_add, {self._sub(2), self._sub(3)})

    def test_pure_remove(self) -> None:
        old = (self._sub(1), self._sub(2), self._sub(3))
        new = (self._sub(1),)
        to_remove, to_add = bridge.diff_subscriptions(old, new)
        self.assertEqual(to_remove, {self._sub(2), self._sub(3)})
        self.assertEqual(to_add, set())

    def test_swap_adds_and_removes(self) -> None:
        # The depth rebalancer's typical pattern: swap one ATM contract
        # for the next as spot drifts. Old:1,2 New:1,3 → remove 2 add 3.
        old = (self._sub(1), self._sub(2))
        new = (self._sub(1), self._sub(3))
        to_remove, to_add = bridge.diff_subscriptions(old, new)
        self.assertEqual(to_remove, {self._sub(2)})
        self.assertEqual(to_add, {self._sub(3)})

    def test_full_replacement(self) -> None:
        old = (self._sub(1), self._sub(2))
        new = (self._sub(3), self._sub(4))
        to_remove, to_add = bridge.diff_subscriptions(old, new)
        self.assertEqual(to_remove, {self._sub(1), self._sub(2)})
        self.assertEqual(to_add, {self._sub(3), self._sub(4)})


class CredentialLoadingTests(unittest.TestCase):
    def test_env_takes_precedence_over_cache(self) -> None:
        import os

        os.environ["DHAN_CLIENT_ID"] = "ENV_CID"
        os.environ["DHAN_ACCESS_TOKEN"] = "ENV_TOKEN"
        try:
            cid, tok = bridge.load_credentials()
            self.assertEqual(cid, "ENV_CID")
            self.assertEqual(tok, "ENV_TOKEN")
        finally:
            del os.environ["DHAN_CLIENT_ID"]
            del os.environ["DHAN_ACCESS_TOKEN"]


if __name__ == "__main__":
    unittest.main()
