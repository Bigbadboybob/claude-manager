"""Bounded, authenticated keyset cursors for the future roster endpoint.

This module does not grant visibility. Callers must calculate authorization
again for every page and bind the cursor to that independently derived scope.
The payload exposes only the last sort key and a digest of the binding; it is
not encrypted. Rotating the signing key invalidates outstanding cursors.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json

MAX_CURSOR = 4096
MAX_KEY_PART = 300


class InvalidCursor(ValueError):
    pass


def _canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=True, allow_nan=False).encode("ascii")


def _binding(context):
    return hashlib.sha256(_canonical(context)).hexdigest()


def _encode(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _decode(data):
    try:
        return base64.b64decode(data + "=" * (-len(data) % 4), altchars=b"-_", validate=True)
    except (ValueError, UnicodeEncodeError) as exc:
        raise InvalidCursor("invalid cursor encoding") from exc


def _valid_key(key):
    return (isinstance(key, list) and len(key) == 3 and
            all(isinstance(part, str) and 0 < len(part) <= MAX_KEY_PART for part in key))


def issue(key, context, secret: bytes) -> str:
    if not _valid_key(key) or not secret:
        raise ValueError("invalid cursor key or signing secret")
    payload = _encode(_canonical({"v": 1, "key": key, "binding": _binding(context)}))
    mac = _encode(hmac.digest(secret, payload.encode("ascii"), "sha256"))
    cursor = payload + "." + mac
    if len(cursor) > MAX_CURSOR:
        raise ValueError("cursor exceeds limit")
    return cursor


def read(cursor: str, context, secret: bytes) -> list[str]:
    if not isinstance(cursor, str) or len(cursor) > MAX_CURSOR or not secret:
        raise InvalidCursor("invalid cursor")
    try:
        payload, signature = cursor.split(".")
        expected = hmac.digest(secret, payload.encode("ascii"), "sha256")
        if not hmac.compare_digest(expected, _decode(signature)):
            raise InvalidCursor("invalid cursor signature")
        obj = json.loads(_decode(payload))
        if (not isinstance(obj, dict) or set(obj) != {"v", "key", "binding"}
                or obj["v"] != 1 or not _valid_key(obj["key"])
                or not hmac.compare_digest(obj["binding"], _binding(context))):
            raise InvalidCursor("cursor scope or filters changed")
        return obj["key"]
    except (ValueError, TypeError, UnicodeEncodeError, KeyError) as exc:
        if isinstance(exc, InvalidCursor):
            raise
        raise InvalidCursor("invalid cursor") from exc
