"""`POST /queues/{queue}/requeue` requires an EXPLICIT selection.

A blanket requeue re-pends every claimed item in the queue — including the
batch an in-flight Consumer fire has just claimed but not yet delivered, which
hands the same items to a second orchestrator. Two ways to reach that blanket
branch used to exist by accident:

  * a bare `POST .../requeue` with no body ("no ids = all"), the obvious thing
    to type when unsticking a queue by hand;
  * `{"ids": []}`, because `dispatch.db.requeue_queue_items` tested `if ids:`
    and so read an empty list as "everything".

Now the selection is always explicit: `{"ids": [...]}` (an empty list requeues
nothing) or `{"all": true}`. Anything else is a 400.
"""

from __future__ import annotations

import asyncio
import os
import unittest
from unittest import mock

import pytest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

# Cloud-only deps — skip cleanly when the FastAPI stack isn't installed.
pytest.importorskip("fastapi")
pytest.importorskip("asyncpg")

from fastapi import HTTPException  # noqa: E402

from api import main as api_main  # noqa: E402
from dispatch import db as dispatch_db  # noqa: E402


def _requeue(body):
    """Call the endpoint coroutine directly with a stand-in pool."""
    return asyncio.run(
        api_main.requeue_queue_batch("props", body=body, pool=object())
    )


class RequeueSelectionTests(unittest.TestCase):
    def test_explicit_ids_requeue_only_those(self):
        with mock.patch.object(
            api_main.db, "requeue_queue_items", new=mock.AsyncMock(return_value=2)
        ) as flip:
            self.assertEqual(_requeue({"ids": ["a", "b"]}), {"requeued": 2})
        self.assertEqual(flip.await_args.args[2], ["a", "b"])

    def test_explicit_all_requeues_everything(self):
        with mock.patch.object(
            api_main.db, "requeue_queue_items", new=mock.AsyncMock(return_value=9)
        ) as flip:
            self.assertEqual(_requeue({"all": True}), {"requeued": 9})
        self.assertIsNone(
            flip.await_args.args[2], "ids=None is the DB helper's all branch"
        )

    def test_empty_body_is_rejected(self):
        for body in (None, {}, {"all": False}):
            with mock.patch.object(
                api_main.db, "requeue_queue_items", new=mock.AsyncMock(return_value=99)
            ) as flip:
                with self.assertRaises(HTTPException) as raised:
                    _requeue(body)
            self.assertEqual(raised.exception.status_code, 400, f"body={body!r}")
            self.assertIn("explicit selection", raised.exception.detail)
            flip.assert_not_awaited()

    def test_ids_and_all_together_are_rejected(self):
        with self.assertRaises(HTTPException) as raised:
            _requeue({"ids": ["a"], "all": True})
        self.assertEqual(raised.exception.status_code, 400)

    def test_malformed_selection_types_are_rejected(self):
        for body in ({"ids": "a"}, {"ids": [1]}, {"all": "yes"}):
            with self.assertRaises(HTTPException) as raised:
                _requeue(body)
            self.assertEqual(raised.exception.status_code, 400, f"body={body!r}")


class RequeueDbEmptyListTests(unittest.IsolatedAsyncioTestCase):
    """`ids=[]` means "these zero items", never "everything"."""

    async def test_empty_id_list_touches_nothing(self):
        pool = mock.MagicMock()
        pool.acquire.side_effect = AssertionError("must not open a connection")
        self.assertEqual(
            await dispatch_db.requeue_queue_items(pool, "props", []), 0
        )

    async def test_none_requeues_all_claimed(self):
        conn = mock.AsyncMock()
        conn.execute.return_value = "UPDATE 4"
        pool = mock.MagicMock()
        pool.acquire.return_value.__aenter__.return_value = conn

        self.assertEqual(
            await dispatch_db.requeue_queue_items(pool, "props", None), 4
        )
        sql = conn.execute.await_args.args[0]
        self.assertNotIn("ANY($2::uuid[])", sql, "the all branch is unscoped by id")


if __name__ == "__main__":
    unittest.main()
