"""Bounded framed RPC on the local daemon socket; no browser-supplied methods."""
import asyncio
import json
import struct
import time
import uuid


class DaemonError(Exception):
    def __init__(self, code, message):
        self.code, self.message = code, message
        super().__init__(message)


class DaemonClient:
    def __init__(self, settings):
        self.settings = settings
        self.slots = asyncio.Semaphore(4)

    async def call(self, method, params, *, timeout=30, feed=False):
        if feed:
            return await self._call(method, params, timeout)
        deadline = time.monotonic() + timeout
        try:
            async with asyncio.timeout(timeout):
                await self.slots.acquire()
        except TimeoutError as exc:
            raise DaemonError("daemon_busy", "The daemon request queue is busy.") from exc
        try:
            return await self._call(method, params, max(0, deadline - time.monotonic()))
        finally:
            self.slots.release()

    async def _call(self, method, params, timeout):
        writer = None
        sent = False
        try:
            async with asyncio.timeout(timeout):
                token = self.settings.token_path.read_text().strip()
                if not token:
                    raise DaemonError("daemon_unavailable", "The daemon board token is unavailable.")
                request = json.dumps({"id": str(uuid.uuid4()), "caller": {"token_id": token},
                                      "method": method, "params": params}).encode()
                reader, writer = await asyncio.open_unix_connection(str(self.settings.socket_path))
                writer.write(struct.pack(">I", len(request)) + request)
                await writer.drain()
                sent = True
                length = struct.unpack(">I", await reader.readexactly(4))[0]
                if length > 8 * 1024 * 1024:
                    raise DaemonError("daemon_protocol", "Oversized daemon response.")
                reply = json.loads(await reader.readexactly(length))
                if not reply.get("ok"):
                    error = reply.get("error", {})
                    raise DaemonError(error.get("code", "daemon_error"), error.get("message", "Daemon refused the request."))
                return reply["result"]
        except (TimeoutError, OSError, asyncio.IncompleteReadError) as exc:
            code = "outcome_unknown" if sent else "daemon_unavailable"
            message = "Daemon response unavailable. Keep the same request ID when retrying." if sent else "Local daemon unavailable."
            raise DaemonError(code, message) from exc
        except (ValueError, KeyError) as exc:
            raise DaemonError("daemon_protocol", "Invalid daemon response.") from exc
        finally:
            if writer:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass
