#!/usr/bin/env python3
"""Unit tests for the 2026-07-13 capture-archive verified S3 offload (prod
disk-full remediation; review round 1 applied — rotation-at-open moved to
RUST, see crates/app/src/groww_bridge.rs::rotate_stale_groww_capture_at_open,
so these tests cover the sidecar-side sweep + pure gates only).

Runnable WITHOUT the growwapi SDK installed:
    cd scripts/groww-sidecar && python3 -m unittest test_capture_archive -v

A stub `growwapi` module is injected before importing groww_sidecar (same
pattern as test_dedup.py). The F4 sweep tests exercise `_archive_sweep`'s
ACTUAL delete path against a stubbed S3 client injected via the
`s3_client_factory` parameter — never real S3.
"""
import os
import sys
import tempfile
import time
import types
import unittest
from unittest import mock

if "growwapi" not in sys.modules:
    _stub = types.ModuleType("growwapi")
    _stub.GrowwAPI = object
    _stub.GrowwFeed = object
    sys.modules["growwapi"] = _stub

import groww_sidecar  # noqa: E402  (stub must precede)
from groww_sidecar import (  # noqa: E402
    ARCHIVE_DELETE_GRACE_SECS,
    ARCHIVE_S3_BUCKET_ENV,
    ARCHIVE_S3_PREFIX_ENV,
    _archive_delete_eligible,
    _archive_path,
    _archive_sweep,
    _collision_free_archive_path,
    _ist_day,
    _list_capture_archives,
)


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


class ListCaptureArchives(unittest.TestCase):
    def test_list_capture_archives_excludes_live_and_foreign_files(self):
        with tempfile.TemporaryDirectory() as d:
            live = os.path.join(d, "live-ticks.ndjson")
            keep = os.path.join(d, "live-ticks-20260712.ndjson")
            for p in (live, keep, os.path.join(d, "bridge-offset.json"),
                      os.path.join(d, "rust-live-ticks.ndjson")):
                open(p, "w", encoding="utf-8").close()
            self.assertEqual(_list_capture_archives(live), [keep])


class _StubS3:
    """Minimal boto3-S3-shaped stub: an in-memory object store with
    injectable upload/head failure modes."""

    def __init__(self, fail_upload=False, head_size_delta=0):
        self.objects = {}
        self.fail_upload = fail_upload
        self.head_size_delta = head_size_delta
        self.uploads = []

    def upload_file(self, path, bucket, key):
        if self.fail_upload:
            raise RuntimeError("stub: upload refused")
        self.uploads.append(key)
        self.objects[(bucket, key)] = os.path.getsize(path)

    def head_object(self, Bucket, Key):  # noqa: N803 - boto3 casing
        size = self.objects.get((Bucket, Key))
        if size is None:
            raise KeyError("stub: no such object")
        return {"ContentLength": size + self.head_size_delta}


class ArchiveSweepDeletePath(unittest.TestCase):
    """F4 (review round 1): pin the verify-before-delete invariant at the
    ACTUAL `_archive_sweep` call site — a hostile `verified = True` revert
    must fail these, not only the pure-gate tests."""

    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.base = os.path.join(self._dir.name, "live-ticks.ndjson")
        self.archive = os.path.join(self._dir.name, "live-ticks-20260712.ndjson")
        with open(self.archive, "w", encoding="utf-8") as fh:
            fh.write('{"ltp":1}\n' * 10)
        # Backdate mtime past any grace so age is never the reason a
        # "kept" assertion passes.
        stale = time.time() - 3 * 86400
        os.utime(self.archive, (stale, stale))
        groww_sidecar._ARCHIVE_WARNED.clear()
        self._env = mock.patch.dict(
            os.environ,
            {ARCHIVE_S3_BUCKET_ENV: "stub-bucket", ARCHIVE_S3_PREFIX_ENV: "stub-prefix"},
        )
        self._env.start()

    def tearDown(self):
        self._env.stop()
        self._dir.cleanup()

    def _sweep(self, s3, grace_secs=0.0, process_start_secs=None):
        _archive_sweep(
            self.base,
            s3_client_factory=lambda: s3,
            grace_secs=grace_secs,
            sleep_fn=lambda _s: None,  # never really sleep in tests
            process_start_secs=process_start_secs,
        )

    def test_upload_failure_keeps_file(self):
        s3 = _StubS3(fail_upload=True)
        self._sweep(s3, grace_secs=0.0)
        self.assertTrue(os.path.exists(self.archive), "upload failed => file KEPT")

    def test_head_size_mismatch_keeps_file(self):
        s3 = _StubS3(head_size_delta=7)  # object exists but wrong size
        self._sweep(s3, grace_secs=0.0)
        self.assertTrue(
            os.path.exists(self.archive),
            "head_object size mismatch => NOT verified => file KEPT",
        )

    def test_verified_and_eligible_deletes_file(self):
        s3 = _StubS3()
        self._sweep(s3, grace_secs=0.0)
        self.assertFalse(
            os.path.exists(self.archive),
            "verified S3 copy + both grace legs => local file deleted",
        )
        self.assertEqual(s3.uploads, ["stub-prefix/live-ticks-20260712.ndjson"])

    def test_verified_but_uptime_below_grace_keeps_file(self):
        s3 = _StubS3()
        # Process "started" just now => uptime ~0 < grace.
        self._sweep(s3, grace_secs=3600.0, process_start_secs=time.time())
        self.assertTrue(
            os.path.exists(self.archive),
            "verified but uptime below grace => KEPT (bridge boot-drain window)",
        )

    def test_verified_but_age_below_grace_keeps_file(self):
        s3 = _StubS3()
        now = time.time()
        os.utime(self.archive, (now, now))  # freshly rotated
        self._sweep(s3, grace_secs=3600.0, process_start_secs=now - 7200)
        self.assertTrue(
            os.path.exists(self.archive),
            "verified but file age below grace => KEPT",
        )

    def test_symlink_is_never_uploaded_or_deleted(self):
        target = os.path.join(self._dir.name, "outside.txt")
        with open(target, "w", encoding="utf-8") as fh:
            fh.write("outside the capture dir\n")
        link = os.path.join(self._dir.name, "live-ticks-20260711.ndjson")
        os.symlink(target, link)
        s3 = _StubS3()
        self._sweep(s3, grace_secs=0.0)
        self.assertTrue(os.path.lexists(link), "symlink must never be deleted")
        self.assertTrue(os.path.exists(target))
        self.assertNotIn("stub-prefix/live-ticks-20260711.ndjson", s3.uploads)

    def test_no_bucket_keeps_everything(self):
        with mock.patch.dict(os.environ, {ARCHIVE_S3_BUCKET_ENV: ""}):
            self._sweep(_StubS3(), grace_secs=0.0)
        self.assertTrue(os.path.exists(self.archive))


class EnvNames(unittest.TestCase):
    def test_env_var_names_match_supervisor_contract(self):
        self.assertEqual(ARCHIVE_S3_BUCKET_ENV, "TICKVAULT_GROWW_ARCHIVE_S3_BUCKET")
        self.assertEqual(ARCHIVE_S3_PREFIX_ENV, "TICKVAULT_GROWW_ARCHIVE_S3_PREFIX")


class IstDaySanity(unittest.TestCase):
    def test_ist_day_boundary(self):
        # 18:30:00 UTC is IST midnight — the day ordinal must roll there.
        at_midnight_ist = ((20650 * 86400) - 19800)  # UTC secs of an IST midnight
        self.assertEqual(_ist_day(at_midnight_ist) - _ist_day(at_midnight_ist - 1), 1)


if __name__ == "__main__":
    unittest.main()
