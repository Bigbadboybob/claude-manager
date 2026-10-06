"""Shared launch receipt polling for blocking calls and background monitors."""

import asyncio
import time

from mcp_server import control_client


async def await_launch_confirmation(
    session_uid: str,
    receipt: dict,
    *,
    socket_path,
    timeout_s: float = 360.0,
    interval: float = 0.5,
) -> dict:
    """Poll the receipt on the exact daemon that spawned this process.

    A missing/replaced receipt (including brain adoption) is not confirmation.
    The daemon owns the one guarded retry; the MCP layer never re-pastes.
    """
    receipt = dict(receipt)
    receipt_id = receipt.get("id")
    deadline = time.monotonic() + timeout_s

    def failed(reason: str) -> dict:
        return {
            **receipt,
            "status": "unconfirmed",
            "submitted": False,
            "confirmed_by": None,
            "reason": reason,
        }

    if not receipt_id:
        return failed("invalid_confirmation_receipt")
    while True:
        if (
            receipt.get("status") in ("confirmed", "delivered")
            and receipt.get("submitted") is True
        ):
            return receipt
        if receipt.get("status") == "unconfirmed":
            return {**receipt, "submitted": False}
        if receipt.get("status") != "pending":
            return failed("invalid_confirmation_receipt")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return failed("confirmation_timeout")
        try:
            resolved = await asyncio.to_thread(
                control_client.call,
                "resolve_authorized_session",
                {"session_uid": session_uid},
                socket_path=socket_path,
                timeout=min(5.0, remaining),
            )
        except control_client.TransportError:
            # A loaded/restarting daemon can miss one short poll. Preserve
            # the obligation until the overall deadline, without re-sending.
            await asyncio.sleep(min(interval, max(0.0, deadline - time.monotonic())))
            continue
        except control_client.ControlError as exc:
            if exc.code in ("conflict", "internal", "busy"):
                await asyncio.sleep(
                    min(interval, max(0.0, deadline - time.monotonic()))
                )
                continue
            return failed("confirmation_unavailable")
        if not isinstance(resolved, dict) or resolved.get("state") == "exited":
            return failed("session_gone")
        current = resolved.get("prompt_delivery")
        if not isinstance(current, dict) or current.get("id") != receipt_id:
            return failed("confirmation_lost_or_session_replaced")
        receipt = dict(current)
        if receipt.get("status") == "pending":
            await asyncio.sleep(min(interval, max(0.0, deadline - time.monotonic())))
