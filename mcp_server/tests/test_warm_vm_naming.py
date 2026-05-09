"""Regression test for warm-VM name uniqueness.

Background: ``_maintain_warm_pools`` may request multiple VMs per pool when
``pool_size > 1``. Until this fix the instance name was derived solely from
the pool id, so the second ``gcloud compute instances create`` for the same
pool always 409'd and the slot stayed unfilled. This test pins the helper
that builds the name to keep emitting distinct values for the same pool.
"""

from __future__ import annotations

import os
import unittest
import uuid

# `dispatch.config` (imported transitively by `api.dispatch_daemon`) hard-fails
# at import time if these env vars are missing. Stub them so the helper under
# test is reachable in a unit-test context without a live DB.
os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

from api.dispatch_daemon import _warm_vm_name  # noqa: E402


class WarmVmNameUniqueTest(unittest.TestCase):
    def test_two_inserts_for_same_pool_get_different_names(self):
        pool_id = uuid.uuid4()
        names = {_warm_vm_name(pool_id) for _ in range(20)}
        # All 20 must be distinct — otherwise the second gcloud insert
        # for a pool will collide and the warm slot will never fill.
        self.assertEqual(len(names), 20)

    def test_name_carries_pool_prefix(self):
        pool_id = uuid.UUID("12345678-aaaa-bbbb-cccc-1234567890ab")
        name = _warm_vm_name(pool_id)
        # Keep the existing operator-friendly prefix so warm VMs stay
        # easy to spot in `gcloud compute instances list`.
        self.assertTrue(name.startswith("cm-warm-12345678-"), name)


if __name__ == "__main__":
    unittest.main()
