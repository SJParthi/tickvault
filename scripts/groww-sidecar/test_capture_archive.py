#!/usr/bin/env python3
"""Unit tests for the 2026-07-13 capture rotation-at-open + verified S3
offload (prod disk-full remediation).

Runnable WITHOUT the growwapi SDK installed:
    cd scripts/groww-sidecar && python3 -m unittest test_capture_archive -v

A stub `growwapi` module is injected before importing groww_sidecar (same
pattern as test_dedup.py). Tests exercise ONLY the pure gates + the local
rotate-at-open filesystem path — never the SDK surface, never real S3.
"""
import os
import sys
import tempfile
import time
import types
import unittest

if "growwapi" not in sys.modules:
    _stub = types.ModuleType("growwapi")
    _stub.GrowwAPI = object
    _stub.GrowwFeed = object
    sys.modules["growwapi"] = _stub

from groww_sidecar import (  # noqa: E402  (stub must precede)
    ARCHIVE_DELETE_GRACE_SECS,
    ARCHIVE_S3_BUCKET_ENV,
    ARCHIVE_S3_PREFIX_ENV,
    _archive_delete_eligible,
    _archive_path,
    _collision_free_archive_path,
    _ist_day,
    _list_capture_archives,
    _rotate_at_open_if_stale,
    _should_rotate_at_open,
)


class ShouldRotateAtOpen(unittest.TestCase):
    def test_should_rotate_at_open_previous_day(self):
        self.assertTrue(_should_rotate_at_open(20649, 20650))
        self.assertTrue(_should_rotate_at_open(20600, 20650))

    def test_should_rotate_at_open_same_day(self):
        # An in-day sidecar restart must APPEND, never rotate.
        self.assertFalse(_should_rotate_at_open(20650, 20650))

    def test_should_rotate_at_open_future_mtime_clock_skew(self):
        # A future mtime (clock skew) must never rotate today's capture out.
        self.assertFalse(_should_rotate_at_open(20651, 20650))


class CollisionFreeArchivePath(unittest.TestCase):
    def test_no_collision_uses_dated_name(self):
        base = "/x/live-ticks.ndjson"
        p = _collision_free_archive_path(base, 20650, lambda _p: False)
        self.assertEqual(p, _archive_path(base, 20650))

    def test_collision_free_archive_path_suffixes(self):
        base = "/x/live-ticks.ndjson"
        dated = _archive_path(base, 20650)
        root, ext = os.path.splitext(dated)
        existing = {dated, f"{root}-1{ext}"}
        p = _collision_free_archive_path(base, 20650, lambda q: q in existing)
        self.assertEqual(p, f"{root}-2{ext}")
        # NEVER overwrite: the returned path is not an existing one.
        self.assertNotIn(p, existing)


class ArchiveDeleteEligible(unittest.TestCase):
    def test_archive_delete_eligible_requires_s3_verified(self):
        # NEVER delete without the verified S3 copy — even with huge graces.
        self.assertFalse(_archive_delete_eligible(False, 1e9, 1e9, ARCHIVE_DELETE_GRACE_SECS))

    def test_archive_delete_eligible_requires_both_graces(self):
        g = float(ARCHIVE_DELETE_GRACE_SECS)
        # age too fresh
        self.assertFalse(_archive_delete_eligible(True, g - 1, g + 1, g))
        # uptime too fresh (the bridge boot-drain protection leg)
        self.assertFalse(_archive_delete_eligible(True, g + 1, g - 1, g))
        # both legs satisfied + verified
        self.assertTrue(_archive_delete_eligible(True, g, g, g))

    def test_grace_constant_is_at_least_45_minutes(self):
        self.assertGreaterEqual(ARCHIVE_DELETE_GRACE_SECS, 45 * 60)


class RotateAtOpenFilesystem(unittest.TestCase):
    def test_rotate_at_open_moves_stale_file(self):
        with tempfile.TemporaryDirectory() as d:
            live = os.path.join(d, "live-ticks.ndjson")
            with open(live, "w", encoding="utf-8") as fh:
                fh.write('{"ltp":1}\n')
            # Backdate mtime 2 IST days.
            stale = time.time() - 2 * 86400
            os.utime(live, (stale, stale))
            archived = _rotate_at_open_if_stale(live)
            self.assertIsNotNone(archived)
            self.assertFalse(os.path.exists(live))
            self.assertTrue(os.path.exists(archived))
            expected = _archive_path(live, _ist_day(stale))
            self.assertEqual(archived, expected)
            self.assertEqual(_list_capture_archives(live), [archived])

    def test_rotate_at_open_leaves_today_file(self):
        with tempfile.TemporaryDirectory() as d:
            live = os.path.join(d, "live-ticks.ndjson")
            with open(live, "w", encoding="utf-8") as fh:
                fh.write('{"ltp":1}\n')
            self.assertIsNone(_rotate_at_open_if_stale(live))
            self.assertTrue(os.path.exists(live))
            self.assertEqual(_list_capture_archives(live), [])

    def test_rotate_at_open_ignores_missing_and_empty(self):
        with tempfile.TemporaryDirectory() as d:
            live = os.path.join(d, "live-ticks.ndjson")
            self.assertIsNone(_rotate_at_open_if_stale(live))  # missing
            open(live, "w", encoding="utf-8").close()  # empty
            stale = time.time() - 2 * 86400
            os.utime(live, (stale, stale))
            self.assertIsNone(_rotate_at_open_if_stale(live))
            self.assertTrue(os.path.exists(live))

    def test_rotate_at_open_collision_appends_suffix(self):
        with tempfile.TemporaryDirectory() as d:
            live = os.path.join(d, "live-ticks.ndjson")
            stale = time.time() - 2 * 86400
            dated = _archive_path(live, _ist_day(stale))
            with open(dated, "w", encoding="utf-8") as fh:
                fh.write("prior archive — must not be overwritten\n")
            with open(live, "w", encoding="utf-8") as fh:
                fh.write('{"ltp":2}\n')
            os.utime(live, (stale, stale))
            archived = _rotate_at_open_if_stale(live)
            root, ext = os.path.splitext(dated)
            self.assertEqual(archived, f"{root}-1{ext}")
            with open(dated, encoding="utf-8") as fh:
                self.assertIn("must not be overwritten", fh.read())

    def test_list_capture_archives_excludes_live_and_foreign_files(self):
        with tempfile.TemporaryDirectory() as d:
            live = os.path.join(d, "live-ticks.ndjson")
            keep = os.path.join(d, "live-ticks-20260712.ndjson")
            for p in (live, keep, os.path.join(d, "bridge-offset.json"),
                      os.path.join(d, "rust-live-ticks.ndjson")):
                open(p, "w", encoding="utf-8").close()
            self.assertEqual(_list_capture_archives(live), [keep])


class EnvNames(unittest.TestCase):
    def test_env_var_names_match_supervisor_contract(self):
        self.assertEqual(ARCHIVE_S3_BUCKET_ENV, "TICKVAULT_GROWW_ARCHIVE_S3_BUCKET")
        self.assertEqual(ARCHIVE_S3_PREFIX_ENV, "TICKVAULT_GROWW_ARCHIVE_S3_PREFIX")


if __name__ == "__main__":
    unittest.main()
