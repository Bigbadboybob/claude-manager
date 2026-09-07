"""Claude's own-child session socket. Never changes inbound policy."""

import asyncio
import json
import os
import time
from pathlib import Path

from mcp_server import control_client
from mcp_server.notifications import NotSubmitted, Queue, consume, transcript_observed


class ClaudeSocket:
    engine = "claude-code"

    def __init__(self, uid: str):
        self.uid = uid
        self.socket = os.environ["CLAUDE_CODE_MESSAGING_SOCKET"].removeprefix("uds:")
        self.token = os.environ.get("CLAUDE_CODE_MESSAGING_TOKEN")
        try:
            self.peer_start = (
                Path(f"/proc/{os.getpid()}/stat")
                .read_text()
                .rsplit(") ", 1)[1]
                .split()[19]
            )
        except (OSError, IndexError):
            self.peer_start = None
        self.transcript_path = None
        self.resolve_after = 0.0

    def identity(self):
        # No socket credentials in persistent diagnostics.
        return {
            "session_uid": self.uid,
            "adapter": "claude-own-child-v1",
            "peer_pid": os.getpid(),
            "peer_start": self.peer_start,
        }

    async def send(self, event):
        try:
            _, writer = await asyncio.wait_for(
                asyncio.open_unix_connection(self.socket), 3
            )
        except (TimeoutError, OSError) as exc:
            raise NotSubmitted from exc
        try:
            if self.token:
                writer.write(
                    (json.dumps({"type": "auth", "token": self.token}) + "\n").encode()
                )
            writer.write(
                (
                    json.dumps(
                        {
                            "type": "user",
                            "from": "claude-manager",
                            "message": {"role": "user", "content": event["text"]},
                        }
                    )
                    + "\n"
                ).encode()
            )
            await asyncio.wait_for(writer.drain(), 3)
            # This interface has no acceptance ACK. Receipt stays unverified
            # until the marker occurs in the actual inbound transcript.
            return {"kind": "socket_write_only"}
        finally:
            writer.close()
            await writer.wait_closed()

    async def observed(self, event):
        # Re-resolve the live binding, but share one lookup across outstanding
        # receipts. A daemon outage must not cost a timeout per queued event.
        if time.monotonic() >= self.resolve_after:
            try:
                resolved = await asyncio.to_thread(
                    control_client.call,
                    "resolve_authorized_session",
                    {"session_uid": self.uid},
                    timeout=2,
                )
                self.transcript_path = resolved.get("transcript_path")
            except (control_client.ControlError, control_client.TransportError):
                self.transcript_path = None
            self.resolve_after = time.monotonic() + 1
        return await asyncio.to_thread(
            transcript_observed,
            self.transcript_path,
            self.engine,
            event["marker"],
            event.get("binding"),
        )


async def run():
    queue = Queue.own()
    adapter = ClaudeSocket(queue.uid)
    while True:
        try:
            # A nested launcher can inherit an ancestor's socket environment.
            # Confirm this CM identity is Claude before consuming its queue.
            resolved = await asyncio.to_thread(
                control_client.call,
                "resolve_authorized_session",
                {"session_uid": queue.uid},
                timeout=3,
            )
            if resolved.get("engine") != "claude-code":
                return
            await consume(queue, adapter)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 - isolate adapter failures from unrelated MCP tools
            queue.health(adapter.engine, "error", error=type(exc).__name__)
            await asyncio.sleep(5)
