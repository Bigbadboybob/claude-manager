"""Wire behavior for roster pagination; no service or database required."""

import unittest

from dispatch.session_roster_cursor import InvalidCursor, issue, read


class CursorTests(unittest.TestCase):
    def setUp(self):
        self.secret = b"fixture-only-key"
        self.context = {"caller": "agent:d:u", "generation": 7,
                        "scope": {"member": "mem_a", "revision": 2},
                        "filters": {"host": None, "state": "working"}}

    def test_round_trip_and_canonical_binding(self):
        token = issue(["daemon", "uid", "agent:daemon:uid"], self.context, self.secret)
        reordered = dict(reversed(list(self.context.items())))
        self.assertEqual(read(token, reordered, self.secret), ["daemon", "uid", "agent:daemon:uid"])

    def test_changed_scope_filter_caller_or_secret_is_refused(self):
        token = issue(["d", "u", "agent:d:u"], self.context, self.secret)
        for changed in ({**self.context, "caller": "agent:d:v"},
                        {**self.context, "generation": 8},
                        {**self.context, "scope": {"member": "mem_a", "revision": 3}},
                        {**self.context, "filters": {"host": "d", "state": "working"}}):
            with self.subTest(changed=changed), self.assertRaises(InvalidCursor):
                read(token, changed, self.secret)
        with self.assertRaises(InvalidCursor):
            read(token, self.context, b"other-key")

    def test_tampering_and_unbounded_input_are_refused(self):
        token = issue(["d", "u", "agent:d:u"], self.context, self.secret)
        for invalid in ("x" * 4097, "not-a-cursor", token + "x", "é." + token):
            with self.subTest(invalid=invalid[:20]), self.assertRaises(InvalidCursor):
                read(invalid, self.context, self.secret)
        with self.assertRaises(ValueError):
            issue(["d", "u", "x" * 301], self.context, self.secret)
