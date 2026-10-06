#!/usr/bin/env python3
"""Measure the task-list transfer and server cost: legacy polling vs the feed.

Boots a throwaway PostgreSQL 17 cluster, runs the repo migrations, seeds a
synthetic task list shaped like production on 2026-09-10 (1,424 non-archived
rows, ~2.4 MB of prompts, ~1.3 MB of descriptions), starts the real API with
uvicorn on a free port, then drives it with a raw HTTP client that counts
BYTES ON THE WIRE (Content-Length of gzip/identity bodies) and requests, while
sampling the API process's CPU time (/proc) and the database's transaction and
tuple counters (pg_stat_database) for each scenario:

  legacy      GET /tasks every 5 s (what the pre-feed TUI did), identity + gzip
  cold        first feed request (snapshot)
  idle        long-poll for N seconds with nothing changing
  single      one PATCH status change -> delivered page
  burst       50 changes in quick succession -> pages
  reconnect   client retains its cursor across 30 changes, then catches up
  delete      hard delete -> remove entry
  expired     cursor older than retention -> resnapshot

Prints a Markdown table plus JSON. No production system is touched.
"""

from __future__ import annotations

import argparse
import asyncio
import gzip
import http.client
import json
import os
import random
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
PG_BIN = Path("/usr/lib/postgresql/17/bin")
TOKEN = "measure-token"
WORDS = ("orderbook signal market deploy trader latent worktree merge session daemon "
         "portal review backtest replica scraper timeline probability hazard epoch cursor "
         "prompt description initiative priority backlog running blocked done").split()


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def text(rng: random.Random, n_bytes: int) -> str:
    out = []
    size = 0
    while size < n_bytes:
        w = rng.choice(WORDS)
        out.append(w)
        size += len(w) + 1
    return " ".join(out)


class Client:
    """Raw HTTP/1.1 client that reports wire bytes (headers excluded)."""

    def __init__(self, port: int, gzip_ok: bool = True):
        self.port = port
        self.gzip_ok = gzip_ok
        self.bytes = 0
        self.requests = 0
        self.conn = None

    def _connect(self):
        self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=60)

    def request(self, method: str, path: str, body: dict | None = None):
        if self.conn is None:
            self._connect()
        headers = {"Authorization": f"Bearer {TOKEN}",
                   "Accept-Encoding": "gzip" if self.gzip_ok else "identity"}
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        try:
            self.conn.request(method, path, body=data, headers=headers)
            resp = self.conn.getresponse()
        except (http.client.HTTPException, OSError):
            self._connect()
            self.conn.request(method, path, body=data, headers=headers)
            resp = self.conn.getresponse()
        raw = resp.read()
        self.requests += 1
        self.bytes += len(raw)
        if resp.getheader("Content-Encoding") == "gzip":
            raw = gzip.decompress(raw)
        if resp.status >= 400:
            raise RuntimeError(f"{method} {path} -> {resp.status} {raw[:200]!r}")
        return json.loads(raw) if raw else None

    def feed(self, cursor: dict | None, wait: float = 0.0, limit: int | None = None):
        q = f"/tasks/changes?wait={wait}"
        if cursor:
            q += f"&since={cursor['seq']}&epoch={cursor['epoch']}"
        if limit:
            q += f"&limit={limit}"
        return self.request("GET", q)

    def reset_counters(self):
        self.bytes = 0
        self.requests = 0


class Meter:
    """CPU seconds of the API process + DB transaction/tuple deltas."""

    def __init__(self, pid: int, dsn: str):
        self.pid = pid
        self.dsn = dsn

    def _cpu(self) -> float:
        parts = Path(f"/proc/{self.pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(parts[11]) + int(parts[12])) / os.sysconf("SC_CLK_TCK")

    def _db(self) -> dict:
        import asyncpg

        async def q():
            c = await asyncpg.connect(self.dsn)
            r = await c.fetchrow("SELECT xact_commit, tup_returned, tup_fetched FROM "
                                 "pg_stat_database WHERE datname = current_database()")
            await c.close()
            return dict(r)
        return asyncio.run(q())

    def start(self):
        self.t0 = time.monotonic()
        self.cpu0 = self._cpu()
        self.db0 = self._db()

    def stop(self) -> dict:
        db1 = self._db()
        return {"seconds": round(time.monotonic() - self.t0, 2),
                "api_cpu_s": round(self._cpu() - self.cpu0, 3),
                "db_xacts": db1["xact_commit"] - self.db0["xact_commit"],
                "db_tup_returned": db1["tup_returned"] - self.db0["tup_returned"]}


def wait_http(port: int, deadline: float = 30):
    t0 = time.monotonic()
    while time.monotonic() - t0 < deadline:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            c.request("GET", "/health")
            if c.getresponse().status == 200:
                return
        except OSError:
            pass
        time.sleep(0.2)
    raise SystemExit("API did not come up")


async def seed(dsn: str, n_tasks: int, n_done: int, rng: random.Random):
    import asyncpg
    from dispatch import db
    pool = await asyncpg.create_pool(dsn, min_size=1, max_size=4, init=db._init_connection)
    await db.init_db(pool)
    ids = []
    for i in range(n_tasks):
        status = "done" if i < n_done else rng.choice(["backlog", "backlog", "running", "draft", "blocked"])
        t = await db.add_task(pool, "git@example:repo.git", "main", text(rng, 1700),
                              priority=rng.randint(0, 5), status=status,
                              project=rng.choice(["predictionTrading", "claude-manager"]),
                              name=f"task {i}", description=text(rng, 920),
                              slug=f"task-{i}", is_cloud=False)
        ids.append(str(t["id"]))
    await pool.close()
    return ids


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tasks", type=int, default=1424)
    ap.add_argument("--done", type=int, default=672)
    ap.add_argument("--idle-seconds", type=float, default=60.0)
    ap.add_argument("--legacy-seconds", type=float, default=30.0)
    ap.add_argument("--baseline-seconds", type=float, default=30.0)
    ap.add_argument("--json", type=Path, help="also write the results here")
    args = ap.parse_args()
    if not (PG_BIN / "initdb").exists():
        raise SystemExit("PostgreSQL 17 binaries required")

    rng = random.Random(20260910)
    tmp = tempfile.TemporaryDirectory(prefix="cm-measure-feed-")
    d = Path(tmp.name)
    pg_port = free_port()
    api_port = free_port()
    subprocess.run([str(PG_BIN / "initdb"), "-D", str(d / "data"), "--no-locale",
                    "--encoding=UTF8", "--auth=trust", "-U", "postgres"], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    subprocess.run([str(PG_BIN / "pg_ctl"), "-D", str(d / "data"), "-l", str(d / "pg.log"), "-o",
                    f"-F -k {d} -h 127.0.0.1 -p {pg_port}", "-w", "start"], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    dsn = f"postgresql://postgres@127.0.0.1:{pg_port}/postgres"
    api = None
    results: dict[str, dict] = {}
    try:
        os.environ["CM_DB_DSN"] = dsn
        os.environ["CM_API_TOKEN"] = TOKEN
        ids = asyncio.run(seed(dsn, args.tasks, args.done, rng))
        live = ids  # done rows are non-archived: part of the list, like prod
        env = {**os.environ, "CM_DB_DSN": dsn, "CM_API_TOKEN": TOKEN,
               "CM_TASK_CHANGES_RETENTION_SECS": "5", "CM_TASK_CHANGES_PRUNE_INTERVAL_SECS": "2"}
        api = subprocess.Popen([sys.executable, "-m", "uvicorn", "api.main:app", "--host", "127.0.0.1",
                                "--port", str(api_port), "--log-level", "warning"],
                               cwd=ROOT, env=env, stdout=open(d / "api.log", "wb"),
                               stderr=subprocess.STDOUT)
        wait_http(api_port)
        time.sleep(1.0)  # let the LISTEN connection come up
        meter = Meter(api.pid, dsn)

        # -- baseline: no client at all (dispatch loops + broker probe) -----
        meter.start()
        time.sleep(args.baseline_seconds)
        results["baseline_no_client"] = {**meter.stop(), "requests": 0, "wire_bytes": 0}

        # -- legacy polling ------------------------------------------------
        for label, gz in (("legacy_identity", False), ("legacy_gzip", True)):
            c = Client(api_port, gzip_ok=gz)
            meter.start()
            t_end = time.monotonic() + args.legacy_seconds
            while time.monotonic() < t_end:
                t = time.monotonic()
                body = c.request("GET", "/tasks")
                time.sleep(max(0.0, 5.0 - (time.monotonic() - t)))
            m = meter.stop()
            results[label] = {**m, "requests": c.requests, "wire_bytes": c.bytes,
                              "rows_per_response": len(body),
                              "bytes_per_day_at_5s": int(c.bytes / c.requests * 86400 / 5)}

        # -- feed: cold connect ---------------------------------------------
        c = Client(api_port)
        meter.start()
        snap = c.feed(None)
        m = meter.stop()
        assert snap["reset"] and len(snap["tasks"]) == len(live), (len(snap["tasks"]), len(live))
        cursor = {"seq": snap["cursor"], "epoch": snap["epoch"]}
        cache = {t["id"]: t for t in snap["tasks"]}
        results["feed_cold_snapshot_gzip"] = {**m, "requests": c.requests, "wire_bytes": c.bytes,
                                              "rows": len(snap["tasks"])}

        def apply(page):
            nonlocal cursor
            if page["reset"]:
                cache.clear()
                cache.update({t["id"]: t for t in page["tasks"]})
            for ch in page["changes"]:
                if ch["op"] == "upsert":
                    cache[ch["task_id"]] = ch["task"]
                else:
                    cache.pop(ch["task_id"], None)
            cursor = {"seq": page["cursor"], "epoch": page["epoch"]}

        # -- feed: idle -----------------------------------------------------
        c.reset_counters()
        meter.start()
        t_end = time.monotonic() + args.idle_seconds
        while time.monotonic() < t_end:
            wait = min(25.0, max(0.0, t_end - time.monotonic()))
            page = c.feed(cursor, wait=round(wait, 1))
            assert not page["changes"], page
            apply(page)
        m = meter.stop()
        results["feed_idle"] = {**m, "requests": c.requests, "wire_bytes": c.bytes,
                                "bytes_per_day": int(c.bytes * 86400 / m["seconds"])}

        # -- feed: single status change (latency from PATCH to delivery) ------
        writer = Client(api_port)
        c.reset_counters()
        meter.start()
        t0 = time.monotonic()
        writer.request("PATCH", f"/tasks/{live[0]}", {"status": "running"})
        page = c.feed(cursor, wait=25)
        latency = time.monotonic() - t0
        apply(page)
        m = meter.stop()
        assert cache[live[0]]["status"] == "running"
        results["feed_single_change"] = {**m, "requests": c.requests, "wire_bytes": c.bytes,
                                         "entries": len(page["changes"]),
                                         "patch_to_delivery_ms": int(latency * 1000)}

        # -- feed: burst ----------------------------------------------------
        c.reset_counters()
        meter.start()
        for i in range(50):
            writer.request("PATCH", f"/tasks/{live[1 + i]}", {"priority": 9})
        pages = 0
        while True:
            page = c.feed(cursor, wait=0)
            apply(page)
            pages += 1
            if not page["changes"] and not page["more"]:
                break
        m = meter.stop()
        assert all(cache[live[1 + i]]["priority"] == 9 for i in range(50))
        results["feed_burst_50"] = {**m, "requests": c.requests, "wire_bytes": c.bytes, "pages": pages}

        # -- feed: reconnect with retained cursor ---------------------------
        held = dict(cursor)
        for i in range(30):
            writer.request("PATCH", f"/tasks/{live[100 + i]}", {"priority": 3, "name": f"renamed {i}"})
        c2 = Client(api_port)
        meter.start()
        page = c2.feed(held, wait=0)
        m = meter.stop()
        apply(page)
        assert not page["reset"] and len(page["changes"]) == 30
        assert all(cache[live[100 + i]]["name"] == f"renamed {i}" for i in range(30))
        results["feed_reconnect_30_changes"] = {**m, "requests": c2.requests, "wire_bytes": c2.bytes,
                                                "entries": len(page["changes"])}

        # -- feed: deletion -------------------------------------------------
        c.reset_counters()
        meter.start()
        writer.request("DELETE", f"/tasks/{live[200]}")
        page = c.feed(cursor, wait=25)
        apply(page)
        m = meter.stop()
        assert live[200] not in cache and page["changes"][0]["op"] == "remove"
        results["feed_delete"] = {**m, "requests": c.requests, "wire_bytes": c.bytes}

        # -- consistency check: cache == server list ------------------------
        listed = {t["id"]: t for t in c.request("GET", "/tasks")}
        assert listed == cache, "cache diverged from the server list"

        # -- feed: expired cursor (retention 5 s in this run) ---------------
        stale = dict(cursor)
        writer.request("PATCH", f"/tasks/{live[300]}", {"priority": 1})
        time.sleep(9)  # past retention + prune interval
        c.reset_counters()
        meter.start()
        page = c.feed(stale, wait=0)
        m = meter.stop()
        assert page["reset"], "expired cursor must resnapshot"
        apply(page)
        results["feed_expired_cursor_resnapshot"] = {**m, "requests": c.requests, "wire_bytes": c.bytes}
        listed = {t["id"]: t for t in c.request("GET", "/tasks")}
        assert listed == cache

    finally:
        if api is not None:
            api.terminate()
            try:
                api.wait(10)
            except subprocess.TimeoutExpired:
                api.kill()
        subprocess.run([str(PG_BIN / "pg_ctl"), "-D", str(d / "data"), "-m", "fast", "-w", "stop"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        tmp.cleanup()

    cols = ["seconds", "requests", "wire_bytes", "api_cpu_s", "db_xacts", "db_tup_returned"]
    print("| scenario | " + " | ".join(cols) + " | notes |")
    print("|---|" + "---:|" * len(cols) + "---|")
    for name, r in results.items():
        notes = ", ".join(f"{k}={v}" for k, v in r.items() if k not in cols)
        print(f"| {name} | " + " | ".join(str(r.get(k, "")) for k in cols) + f" | {notes} |")
    if args.json:
        args.json.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
