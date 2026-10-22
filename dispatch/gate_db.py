"""Postgres access for gate receipts (doc/gate-receipts.md).

Every write takes the board row lock (`SELECT … FOR UPDATE`) like the item
writes in `dispatch.items_db`, so receipts, grants, profile edits and the
staleness they cause are ordered with item changes. Flags, pushes and
history go through `BoardTx` and `items_db._apply`, so a receipt event moves
the board version exactly like an item event.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timedelta

from dispatch import gate_rules as gr
from dispatch import items_db
from dispatch.items_rules import CLOSED, OWNER_PID, BoardTx, ItemsError, iso

GateError = gr.GateError
MAX_WATCH = 200
BINDING_FIELDS = ("head", "mf", "ruling", "gate_class")


def _bid(board: dict) -> uuid.UUID:
    return uuid.UUID(board["id"])


# ---- loading ------------------------------------------------------------------

def _row_receipt(row, slug: str, n: int) -> dict:
    """A stored receipt in the normalized shape `gate_rules` evaluates."""
    return {
        "id": row["id"], "item_id": row["item_id"], "n": n, "seq": row["seq"],
        "label": gr.receipt_label(slug, n, row["seq"]),
        "gate": row["gate"], "class": row["class"], "profile_version": row["profile_version"],
        "head_sha": row["head_sha"], "mf_sha": row["mf_sha"], "runner": row["runner"],
        "ruling_id": row["ruling_id"], "base": row["base"] or {}, "closure": row["closure"],
        "worktree": row["worktree"] or {}, "inputs": row["inputs"] or [],
        "steps": row["steps"] or [], "waivers": list(row["waivers"] or []),
        "repo": row["repo"], "branch": row["branch"], "log_path": row["log_path"],
        "attestation": row["attestation"], "computed": row["computed"],
        "verdict": row["verdict"], "stale_at": row["stale_at"],
        "stale_reason": row["stale_reason"], "checked_at": row["checked_at"],
        "poster_pid": row["poster_pid"], "poster_daemon_id": row["poster_daemon_id"],
        "posted_at": row["posted_at"],
    }


def _row_grant(row, slug: str, n: int) -> dict:
    return {
        "id": row["id"], "label": gr.grant_label(slug, n, row["seq"]), "seq": row["seq"],
        "kind": row["kind"], "gate": row["gate"], "grantor_pid": row["grantor_pid"],
        "head_sha": row["head_sha"], "base_sha": row["base_sha"], "step": row["step"],
        "nodes": list(row["nodes"]) if row["nodes"] is not None else None,
        "cmd_digest": row["cmd_digest"], "reason": row["reason"], "risk": row["risk"],
        "message_id": row["message_id"], "message_check": row["message_check"] or {},
        "revoked_at": row["revoked_at"], "created_at": row["created_at"],
    }


def grantors(board: dict) -> set[str]:
    return {OWNER_PID, *(board.get("waiver_grantors") or [])}


def effective_class(board: dict, item_row) -> str | None:
    return (item_row["gate_class"] if item_row else None) or board.get("gate_default_class")


def profile_for(board: dict, klass: str | None) -> dict | None:
    if not klass:
        return None
    return (board.get("gate_profiles") or {}).get(klass)


async def _item_row(conn, board: dict, n: int):
    row = await conn.fetchrow(
        "SELECT id, number, status, closed_at, archived_at, gate_class, gate_binding "
        "FROM items WHERE board_id = $1 AND number = $2", _bid(board), n)
    if row is None:
        raise ItemsError(404, "not_found", f"no item #{n} on board {board['slug']}", n=n)
    return row


async def _grants(conn, board: dict, item_ids: list[int], profiles_by_item: dict) -> dict:
    """item_id -> [grant with label and problems]."""
    out: dict[int, list] = {}
    if not item_ids:
        return out
    rows = await conn.fetch(
        """SELECT g.*, i.number FROM item_grants g JOIN items i ON i.id = g.item_id
            WHERE g.item_id = ANY($1::bigint[]) ORDER BY g.id""", item_ids)
    who = grantors(board)
    for r in rows:
        grant = _row_grant(r, board["slug"], r["number"])
        grant["problems"] = gr.grant_problems(grant, profiles_by_item.get(r["item_id"]), who)
        out.setdefault(r["item_id"], []).append(grant)
    return out


async def _receipts(conn, board: dict, where: str, *args) -> list[dict]:
    rows = await conn.fetch(
        f"""SELECT r.*, i.number FROM item_receipts r JOIN items i ON i.id = r.item_id
             WHERE r.board_id = $1 AND {where} ORDER BY r.id""", _bid(board), *args)
    return [_row_receipt(r, board["slug"], r["number"]) for r in rows]


async def _latest_ids(conn, item_ids: list[int]) -> set[int]:
    rows = await conn.fetch(
        """SELECT DISTINCT ON (item_id, gate) id FROM item_receipts
            WHERE item_id = ANY($1::bigint[]) ORDER BY item_id, gate, id DESC""", item_ids)
    return {r["id"] for r in rows}


async def _locked(conn, ref: str, extra_ns=()) -> tuple[dict, dict]:
    board = await items_db._board_by_ref(conn, ref, lock=True)
    items = await items_db._load_items(conn, board["id"], extra_ns)
    return board, items


# ---- staleness and flags --------------------------------------------------------

def _gate_push(tx: BoardTx, it, kind: str, text: str):
    for h in it.holders:
        tx._push(it, h, kind, f"{tx._prefix()} #{it.n} \"{it.title}\": {text}")


async def _mark_stale(conn, tx: BoardTx, board: dict, receipts: list[dict], stale: dict) -> list[str]:
    """Mark receipts STALE (one-way), flag the items whose latest receipt it
    was, and cascade to receipts that consumed their artifacts."""
    labels: list[str] = []
    queue = [(r, stale) for r in receipts if r.get("stale_at") is None]
    seen: set[int] = set()
    while queue:
        r, why = queue.pop(0)
        if r["id"] in seen:
            continue
        seen.add(r["id"])
        done = await conn.fetchval(
            """UPDATE item_receipts SET stale_at = $2, stale_reason = $3
                WHERE id = $1 AND stale_at IS NULL RETURNING id""", r["id"], tx.now, why)
        if done is None:
            continue
        labels.append(r["label"])
        it = tx.items.get(r["n"])
        if it is not None:
            tx._event(it, "receipt_stale", prev={"receipt": r["label"],
                                                 "verdict": (r.get("verdict") or {}).get("verdict")},
                      new=why)
            if r["id"] in await _latest_ids(conn, [r["item_id"]]) and it.status not in CLOSED:
                detail = dict((it.open_flags.get("receipt_stale") or {}).get("detail") or {})
                gates = dict(detail.get("gates") or {})
                gates[r["gate"]] = {"receipt": r["label"], "reason": why["reason"]}
                fresh = "receipt_stale" not in it.open_flags
                tx.raise_flag(it, "receipt_stale", {"gates": gates})
                if fresh:
                    _gate_push(tx, it, "receipt_stale",
                               f"gate receipt {r['label']} is STALE ({why['reason']} "
                               f"{why.get('prev') or ''} → {why.get('new') or ''}). Rerun the gate "
                               f"(cm-gate run --item {it.n}) before anyone rules on it.")
        consumers = await _receipts(
            conn, board,
            """r.stale_at IS NULL AND r.id IN (
                   SELECT receipt_id FROM item_receipt_inputs WHERE produced_by = $2)""", r["id"])
        for c in consumers:
            queue.append((c, {"reason": "input_receipt_invalid", "prev": r["label"],
                              "new": why["reason"]}))
    return labels


async def _void_flags(conn, tx: BoardTx, board: dict, rec: dict, it):
    """`runner VOID ×N` / `input VOID ×N`, raised on repeats within 24 h."""
    since = tx.now - timedelta(seconds=gr.VOID_WINDOW_S)
    verdict = rec["verdict"]["verdict"]
    if verdict != "VOID":
        for kind in ("runner_void", "input_void"):
            tx.resolve_flag(it, kind, "cleared")
        return
    runner_sha = (rec.get("runner") or {}).get("sha")
    if runner_sha:
        rows = await conn.fetch(
            """SELECT r.seq, i.number FROM item_receipts r JOIN items i ON i.id = r.item_id
                WHERE r.board_id = $1 AND r.runner->>'sha' = $2 AND r.posted_at >= $3
                  AND r.computed->>'verdict' = 'VOID' ORDER BY r.id""",
            _bid(board), runner_sha, since)
        if len(rows) >= gr.VOID_REPEAT:
            fresh = "runner_void" not in it.open_flags
            tx.raise_flag(it, "runner_void", {
                "runner_sha": gr.short(runner_sha), "count": len(rows),
                "receipts": [gr.receipt_label(board["slug"], x["number"], x["seq"]) for x in rows[-5:]]})
            if fresh:
                _gate_push(tx, it, "runner_void",
                           f"runner VOID ×{len(rows)} on runner {gr.short(runner_sha)} in 24 h: "
                           "check the runner before another rerun.")
    for inp in rec["inputs"]:
        if inp["state"] != "missing" or not inp["locator"]:
            continue
        count = await conn.fetchval(
            """SELECT count(*) FROM item_receipt_inputs x JOIN item_receipts r ON r.id = x.receipt_id
                WHERE r.board_id = $1 AND x.locator = $2 AND x.state = 'missing'
                  AND r.posted_at >= $3""", _bid(board), inp["locator"], since)
        if count >= gr.VOID_REPEAT:
            fresh = "input_void" not in it.open_flags
            tx.raise_flag(it, "input_void", {"input": inp["name"], "locator": inp["locator"][:200],
                                             "count": int(count)})
            if fresh:
                _gate_push(tx, it, "input_void",
                           f"input {inp['name']} VOID ×{count} in 24 h ({inp['locator'][:120]}): "
                           "fix the input before another rerun.")
            break


def _clear_stale_flag(tx: BoardTx, it, gate: str):
    flag = it.open_flags.get("receipt_stale")
    if not flag:
        return
    gates = dict((flag.get("detail") or {}).get("gates") or {})
    gates.pop(gate, None)
    if gates:
        tx.raise_flag(it, "receipt_stale", {"gates": gates})
    else:
        tx.resolve_flag(it, "receipt_stale", "cleared")


# ---- posting a receipt -------------------------------------------------------------

def _public(rec: dict, now: datetime, *, steps: bool = True) -> dict:
    stale = rec.get("stale_reason") if rec.get("stale_at") else None
    verdict = rec.get("verdict") or {}
    out = {
        "id": rec["label"], "gate": rec["gate"], "class": rec["class"],
        "verdict": "STALE" if stale else verdict.get("verdict"),
        "was": verdict.get("verdict") if stale else None,
        "headline": gr.headline(verdict, stale),
        "reasons": verdict.get("reasons", []),
        "stale": stale, "stale_at": iso(rec.get("stale_at")),
        "head": rec["head_sha"], "mf": rec["mf_sha"], "runner": rec["runner"],
        "ruling": rec["ruling_id"], "base": rec["base"], "attestation": rec["attestation"],
        "inputs": rec["inputs"], "waivers": rec["waivers"],
        "posted_at": iso(rec["posted_at"]), "poster": rec["poster_pid"],
        "computed": (rec.get("computed") or {}).get("verdict"),
        "checked_at": iso(rec.get("checked_at")),
        "profile_version": rec.get("profile_version"),
    }
    if rec.get("closure"):
        out["closure"] = {k: rec["closure"].get(k) for k in ("digest", "count", "cmd")}
    if not stale and (rec.get("base") or {}).get("sha_after"):
        last = rec.get("checked_at") or rec["posted_at"]
        age = int((now - last).total_seconds())
        if age >= gr.UNCHECKED_AFTER_S:
            out["unchecked_s"] = age
    out["inputs_note"] = "inputs checked at run time"
    if steps:
        out["steps"] = rec["steps"]
        out["worktree"] = rec["worktree"]
        out["log_path"] = rec["log_path"]
        out["repo"], out["branch"] = rec["repo"], rec["branch"]
    else:
        out["steps"] = [{"name": s["name"], "state": s["state"], "rc": s.get("rc"),
                         "parsed": s.get("parsed")} for s in rec["steps"]]
    return out


async def post_receipt(pool, ref: str, actor: dict, n: int, raw: dict, *,
                       request_id: str | None = None) -> dict:
    receipt = gr.normalize_receipt(raw)
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board, items = await _locked(conn, ref, [n])
            if request_id:
                prior = await _receipts(conn, board, "r.request_id = $2", request_id)
                if prior:
                    out = await _reply(conn, board, prior[0], now)
                    out["replayed"] = True
                    return out
            tx = BoardTx(board, items, actor, now)
            it = tx.item(n)
            row = await _item_row(conn, board, n)
            if it.archived_at is not None:
                raise ItemsError(422, "item_closed", f"item #{n} is archived", n=n)
            klass = effective_class(board, row)
            if receipt["class"] and receipt["class"] != klass:
                # The item's class decides the requirements, never the poster.
                raise GateError(422, "class_mismatch",
                                f"item #{n} uses gate class {klass or '(none)'}, not {receipt['class']}",
                                field="class")
            profile = profile_for(board, klass)
            binding = dict(row["gate_binding"] or {})
            grants = (await _grants(conn, board, [row["id"]], {row["id"]: profile})).get(row["id"], [])
            # Produced-by references must name receipts on this board.
            for inp in receipt["inputs"]:
                if inp["produced_by"]:
                    inp["produced_by_id"] = await _resolve_label(conn, board, inp["produced_by"])
            verdict = gr.evaluate(receipt, profile, grants, klass=klass)
            seq = int(await conn.fetchval(
                "SELECT COALESCE(max(seq), 0) + 1 FROM item_receipts WHERE item_id = $1", row["id"]))
            rid = await conn.fetchval(
                """INSERT INTO item_receipts (board_id, item_id, seq, gate, class, profile_version,
                       head_sha, mf_sha, runner, ruling_id, base, closure, worktree, inputs, steps,
                       waivers, repo, branch, log_path, attestation, computed, verdict,
                       poster_pid, poster_daemon_id, request_id, posted_at)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,
                           $21,$21,$22,$23,$24,$25) RETURNING id""",
                _bid(board), row["id"], seq, receipt["gate"], klass,
                (profile or {}).get("version"), receipt["head_sha"], receipt["mf_sha"],
                receipt["runner"], receipt["ruling_id"], receipt["base"], receipt["closure"],
                receipt["worktree"], [{k: v for k, v in i.items() if k != "produced_by_id"}
                                      for i in receipt["inputs"]],
                receipt["steps"], receipt["waivers"], receipt["repo"], receipt["branch"],
                receipt["log_path"], receipt["attestation"], verdict, actor["pid"],
                actor.get("daemon_id"), request_id, now)
            for inp in receipt["inputs"]:
                await conn.execute(
                    """INSERT INTO item_receipt_inputs (receipt_id, name, locator, content_id, state,
                                                        produced_by)
                       VALUES ($1, $2, $3, $4, $5, $6)""",
                    rid, inp["name"], inp["locator"] or f"(unnamed:{inp['name']})",
                    inp["content_id"], inp["state"], inp.get("produced_by_id"))
            rec = (await _receipts(conn, board, "r.id = $2", rid))[0]
            tx._event(it, "receipt_posted", new={
                "receipt": rec["label"], "gate": rec["gate"], "verdict": verdict["verdict"],
                "head": gr.short(rec["head_sha"]), "attestation": rec["attestation"]})
            _clear_stale_flag(tx, it, rec["gate"])
            # Older receipts of this gate on another head or MF are stale now.
            older = await _receipts(conn, board, "r.item_id = $2 AND r.gate = $3 AND r.id <> $4 "
                                    "AND r.stale_at IS NULL", row["id"], rec["gate"], rid)
            for old in older:
                why = gr.superseded_by(old, rec)
                if why:
                    await _mark_stale(conn, tx, board, [old], why)
            # Any receipt on the board that bound another version of an input
            # this run read is stale (input_changed).
            for inp in rec["inputs"]:
                if inp["state"] != "bound" or not inp["locator"]:
                    continue
                others = await _receipts(
                    conn, board,
                    """r.stale_at IS NULL AND r.id <> $2 AND r.id IN (
                           SELECT receipt_id FROM item_receipt_inputs
                            WHERE locator = $3 AND state = 'bound' AND content_id IS DISTINCT FROM $4)""",
                    rid, inp["locator"], inp["content_id"])
                for o in others:
                    await _mark_stale(conn, tx, board, [o], {
                        "reason": "input_changed", "prev": inp["name"], "new": rec["label"]})
            arrival = gr.arrival_stale(rec, binding)
            if arrival:
                await _mark_stale(conn, tx, board, [rec], arrival)
            elif verdict["verdict"] in ("FAIL", "VOID"):
                # Consumers of an artifact a failed run produced cannot stand.
                consumers = await _receipts(
                    conn, board, """r.stale_at IS NULL AND r.id IN (SELECT receipt_id FROM
                        item_receipt_inputs WHERE produced_by = $2)""", rid)
                await _mark_stale(conn, tx, board, consumers, {
                    "reason": "input_receipt_invalid", "prev": rec["label"],
                    "new": verdict["verdict"]})
            await _void_flags(conn, tx, board, rec, it)
            await items_db._apply(conn, tx)
            rec = (await _receipts(conn, board, "r.id = $2", rid))[0]
            return await _reply(conn, board, rec, now)


async def _resolve_label(conn, board: dict, label: str) -> int:
    prefix = f"r-{board['slug']}-"
    try:
        if not label.startswith(prefix):
            raise ValueError
        n, seq = (int(x) for x in label[len(prefix):].rsplit("-", 1))
    except ValueError:
        raise GateError(422, "invalid_field",
                        f"produced_by {label!r} is not a receipt on board {board['slug']}",
                        field="inputs") from None
    rid = await conn.fetchval(
        """SELECT r.id FROM item_receipts r JOIN items i ON i.id = r.item_id
            WHERE r.board_id = $1 AND i.number = $2 AND r.seq = $3""", _bid(board), n, seq)
    if rid is None:
        raise GateError(422, "invalid_field", f"no receipt {label}", field="inputs")
    return rid


async def _reply(conn, board: dict, rec: dict, now: datetime) -> dict:
    counts = await _run_counts(conn, rec["item_id"], rec["gate"], rec["head_sha"])
    out = _public(rec, now)
    out["runs"] = counts
    return {"board": {"id": board["id"], "slug": board["slug"]}, "n": rec["n"], "receipt": out}


async def _run_counts(conn, item_id: int, gate: str, head: str) -> dict:
    rows = await conn.fetch(
        """SELECT computed->>'verdict' AS v, count(*) AS c FROM item_receipts
            WHERE item_id = $1 AND gate = $2 AND head_sha = $3 GROUP BY 1""", item_id, gate, head)
    return {r["v"]: int(r["c"]) for r in rows}


# ---- reading -------------------------------------------------------------------

async def list_receipts(pool, ref: str, n: int, *, gate: str | None = None,
                        history: bool = False) -> dict:
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        board = await items_db._board_by_ref(conn, ref)
        row = await _item_row(conn, board, n)
        klass = effective_class(board, row)
        recs = await _receipts(conn, board, "r.item_id = $2" + (" AND r.gate = $3" if gate else ""),
                               row["id"], *([gate] if gate else []))
        latest: dict[str, dict] = {}
        for r in recs:
            latest[r["gate"]] = r
        out_latest = {}
        for g, r in latest.items():
            pub = _public(r, now)
            pub["runs"] = await _run_counts(conn, row["id"], g, r["head_sha"])
            out_latest[g] = pub
        grants = (await _grants(conn, board, [row["id"]],
                                {row["id"]: profile_for(board, klass)})).get(row["id"], [])
        out = {
            "board": {"id": board["id"], "slug": board["slug"]}, "n": n, "class": klass,
            "binding": dict(row["gate_binding"] or {}), "latest": out_latest,
            "grants": [_grant_public(x) for x in grants],
        }
        if history:
            out["history"] = [_public(r, now, steps=False) for r in reversed(recs)]
        return out


def _grant_public(grant: dict) -> dict:
    return {
        "id": grant["label"], "kind": grant["kind"], "gate": grant["gate"],
        "grantor": grant["grantor_pid"], "head": grant["head_sha"], "base": grant["base_sha"],
        "step": grant["step"], "nodes": grant["nodes"], "cmd_digest": grant["cmd_digest"],
        "reason": grant["reason"], "risk": grant["risk"], "message_id": grant["message_id"],
        "verified": not grant.get("problems"), "problems": grant.get("problems") or [],
        "revoked_at": iso(grant["revoked_at"]), "created_at": iso(grant["created_at"]),
    }


async def gate_summary(conn, board: dict, item_ids: list[int], now: datetime) -> dict:
    """item_id -> {gate: compact latest receipt}, for board reads."""
    if not item_ids:
        return {}
    rows = await conn.fetch(
        """SELECT DISTINCT ON (r.item_id, r.gate) r.*, i.number FROM item_receipts r
             JOIN items i ON i.id = r.item_id
            WHERE r.item_id = ANY($1::bigint[]) ORDER BY r.item_id, r.gate, r.id DESC""", item_ids)
    out: dict[int, dict] = {}
    for row in rows:
        rec = _row_receipt(row, board["slug"], row["number"])
        pub = _public(rec, now, steps=False)
        out.setdefault(row["item_id"], {})[rec["gate"]] = {
            k: pub.get(k) for k in ("id", "verdict", "was", "headline", "head", "stale",
                                    "unchecked_s", "attestation", "posted_at")
        } | {"reasons": [r for r in pub["reasons"] if r["level"] in gr.LEVELS][:5],
             "steps": pub["steps"]}
    return out


# ---- grants --------------------------------------------------------------------

async def attach_grant(pool, ref: str, actor: dict, n: int, body: dict) -> dict:
    now = items_db.utcnow()
    kind = body.get("kind") or "waiver"
    if kind not in gr.GRANT_KINDS:
        raise gr.bad("kind is waiver or variance", "kind")
    head = gr._sha(body.get("head"), "head", required=True)
    base = gr._sha(body.get("base"), "base")
    step = gr._str(body.get("step"), "step", 40, required=True)
    gate = body.get("gate") or "gate"
    if not gr.GATE_NAME_RE.match(gate):
        raise gr.bad("bad gate name", "gate")
    nodes = gr._str_list(body.get("nodes"), "nodes", 500, 1000) or None
    reason = (gr._str(body.get("reason"), "reason", 500) or "").strip()
    risk = (gr._str(body.get("risk"), "risk", 500) or "").strip()
    if not reason or not risk:
        raise gr.bad("a grant states both the reason and the real risk", "reason")
    message_id = gr._str(body.get("message_id"), "message_id", 200, required=True)
    cmd = gr._str(body.get("cmd_digest"), "cmd_digest", 80)
    if kind == "variance" and not cmd:
        raise gr.bad("a variance grant binds the effective command's cmd_digest", "cmd_digest")
    check = body.get("message_check") or {}
    if not isinstance(check, dict):
        raise gr.bad("message_check is an object", "message_check")
    async with pool.acquire() as conn:
        async with conn.transaction():
            board, items = await _locked(conn, ref, [n])
            if actor["pid"] not in grantors(board):
                raise ItemsError(403, "forbidden",
                                 f"{actor.get('name') or actor['pid']} is not a waiver grantor on "
                                 f"{board['slug']} (Owner sets waiver_grantors)")
            tx = BoardTx(board, items, actor, now)
            it = tx.item(n)
            row = await _item_row(conn, board, n)
            profile = profile_for(board, effective_class(board, row))
            draft = {"kind": kind, "grantor_pid": actor["pid"], "head_sha": head, "base_sha": base,
                     "step": step, "nodes": nodes, "cmd_digest": cmd, "reason": reason,
                     "risk": risk, "message_check": check, "revoked_at": None}
            problems = gr.grant_problems(draft, profile, grantors(board))
            structural = [p for p in problems if p.startswith(("step ", "nodes outside",
                                                                "a variance"))]
            if structural:
                # A generic or mis-scoped grant is refused outright.
                raise GateError(422, "invalid_grant", "; ".join(structural), field="step")
            seq = int(await conn.fetchval(
                "SELECT COALESCE(max(seq), 0) + 1 FROM item_grants WHERE item_id = $1", row["id"]))
            await conn.execute(
                """INSERT INTO item_grants (board_id, item_id, seq, kind, gate, grantor_pid, head_sha,
                       base_sha, step, nodes, cmd_digest, reason, risk, message_id, message_check,
                       created_at)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)""",
                _bid(board), row["id"], seq, kind, gate, actor["pid"], head, base, step, nodes,
                cmd, reason, risk, message_id, check, now)
            label = gr.grant_label(board["slug"], n, seq)
            tx._event(it, "grant_attached", new={"grant": label, "kind": kind, "step": step,
                                                 "head": gr.short(head), "verified": not problems,
                                                 "problems": problems})
            await _reevaluate(conn, tx, board, row, profile)
            await items_db._apply(conn, tx)
            grants = (await _grants(conn, board, [row["id"]], {row["id"]: profile}))[row["id"]]
            return {"board": {"id": board["id"], "slug": board["slug"]}, "n": n,
                    "grant": _grant_public(next(x for x in grants if x["seq"] == seq))}


async def update_grant(pool, ref: str, actor: dict, n: int, label: str, *,
                       revoke: bool = False, message_check: dict | None = None) -> dict:
    """Revoke a grant (its grantor or Owner) or record a fresh message check."""
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board, items = await _locked(conn, ref, [n])
            tx = BoardTx(board, items, actor, now)
            it = tx.item(n)
            row = await _item_row(conn, board, n)
            profile = profile_for(board, effective_class(board, row))
            grants = (await _grants(conn, board, [row["id"]], {row["id"]: profile})).get(row["id"], [])
            grant = next((x for x in grants if x["label"] == label), None)
            if grant is None:
                raise ItemsError(404, "not_found", f"no grant {label} on #{n}")
            if revoke:
                if actor["pid"] not in (grant["grantor_pid"], OWNER_PID):
                    raise ItemsError(403, "forbidden", "only the grantor or Owner revokes a grant")
                if grant["revoked_at"] is None:
                    await conn.execute("UPDATE item_grants SET revoked_at = $2 WHERE id = $1",
                                       grant["id"], now)
                    tx._event(it, "grant_revoked", new={"grant": label})
                    users = [r for r in await _receipts(conn, board, "r.item_id = $2 AND r.stale_at IS NULL",
                                                        row["id"])
                             if label in r["waivers"] or any(label in x["text"] for x in
                                                             (r["verdict"] or {}).get("reasons", []))]
                    await _mark_stale(conn, tx, board, users,
                                      {"reason": "grant_revoked", "prev": label, "new": None})
            elif message_check is not None:
                if not isinstance(message_check, dict):
                    raise gr.bad("message_check is an object", "message_check")
                if message_check != grant["message_check"]:
                    await conn.execute("UPDATE item_grants SET message_check = $2 WHERE id = $1",
                                       grant["id"], message_check)
                    tx._event(it, "grant_checked", new={"grant": label, "check": message_check})
                    await _reevaluate(conn, tx, board, row, profile)
            await items_db._apply(conn, tx)
            grants = (await _grants(conn, board, [row["id"]], {row["id"]: profile}))[row["id"]]
            return {"board": {"id": board["id"], "slug": board["slug"]}, "n": n,
                    "grant": _grant_public(next(x for x in grants if x["label"] == label))}


async def _reevaluate(conn, tx: BoardTx, board: dict, row, profile):
    """Re-run the verdict of an item's live receipts after its grants changed."""
    grants = (await _grants(conn, board, [row["id"]], {row["id"]: profile})).get(row["id"], [])
    it = tx.items.get(row["number"])
    for rec in await _receipts(conn, board, "r.item_id = $2 AND r.stale_at IS NULL", row["id"]):
        prof = profile_for(board, rec["class"])
        if prof is not None and rec.get("profile_version") != prof.get("version"):
            continue  # a profile edit already restaled it; never re-judge under a new profile
        verdict = gr.evaluate(rec, prof, grants, klass=rec["class"])
        if verdict != rec["verdict"]:
            await conn.execute("UPDATE item_receipts SET verdict = $2 WHERE id = $1", rec["id"], verdict)
            if it is not None:
                tx._event(it, "receipt_reevaluated", prev={"receipt": rec["label"],
                                                          "verdict": rec["verdict"]["verdict"]},
                          new={"verdict": verdict["verdict"]})


# ---- profiles ----------------------------------------------------------------------

async def _may_edit_profiles(conn, board: dict, pid: str, now: datetime) -> bool:
    if pid == OWNER_PID or pid in (board.get("gate_editors") or []):
        return True
    orch = await items_db._orchestrator_row(conn, board, now)
    return bool(orch and orch.get("pid") == pid)


async def get_profile(pool, ref: str, *, klass: str | None = None, n: int | None = None) -> dict:
    async with pool.acquire() as conn:
        board = await items_db._board_by_ref(conn, ref)
        binding = None
        if n is not None:
            row = await _item_row(conn, board, n)
            klass = klass or effective_class(board, row)
            binding = dict(row["gate_binding"] or {})
        klass = klass or board.get("gate_default_class")
        out = {"board": {"id": board["id"], "slug": board["slug"]}, "class": klass,
               "profile": profile_for(board, klass),
               "classes": sorted((board.get("gate_profiles") or {}).keys()),
               "default_class": board.get("gate_default_class"),
               "gate_editors": list(board.get("gate_editors") or []),
               "waiver_grantors": list(board.get("waiver_grantors") or [])}
        if binding is not None:
            out["binding"] = binding
        return out


async def set_profile(pool, ref: str, actor: dict, klass: str, profile, *, reason: str,
                      default: bool = False) -> dict:
    """Create, replace or (profile=None) delete a class; logs who, old → new and
    why, and restales every live receipt of the class (EP, 2026-10-21)."""
    if not gr.GATE_NAME_RE.match(klass or ""):
        raise gr.bad("class must be lowercase [a-z0-9_.-], at most 40 characters", "class")
    reason = (reason or "").strip()
    if not reason:
        raise gr.bad("a profile edit needs a reason", "reason")
    norm = gr.validate_profile(profile) if profile is not None else None
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board, items = await _locked(conn, ref)
            if not await _may_edit_profiles(conn, board, actor["pid"], now):
                raise ItemsError(403, "forbidden",
                                 "gate profiles are edited by Owner, the board's orchestrator or a "
                                 "gate editor")
            tx = BoardTx(board, items, actor, now)
            profiles = dict(board.get("gate_profiles") or {})
            old = profiles.get(klass)
            version = (old or {}).get("version", 0) + 1
            if norm is not None:
                norm["version"] = version
                profiles[klass] = norm
            else:
                profiles.pop(klass, None)
            default_class = klass if default and norm is not None else board.get("gate_default_class")
            if norm is None and default_class == klass:
                default_class = None
            await conn.execute(
                "UPDATE boards SET gate_profiles = $2, gate_default_class = $3, updated_at = $4 "
                "WHERE id = $1", _bid(board), profiles, default_class, now)
            tx.events.append({"n": None, "type": "gate_profile_changed",
                              "prev": {"class": klass, "profile": old},
                              "new": {"class": klass, "profile": norm, "default": default},
                              "reason": reason[:500]})
            board["gate_profiles"], board["gate_default_class"] = profiles, default_class
            live = await _receipts(conn, board, "r.class = $2 AND r.stale_at IS NULL", klass)
            restaled = await _mark_stale(conn, tx, board, live, {
                "reason": "profile_changed", "prev": f"{klass} v{version - 1}",
                "new": f"v{version}" if norm is not None else "deleted"})
            await items_db._apply(conn, tx)
            return {"board": {"id": board["id"], "slug": board["slug"]}, "class": klass,
                    "profile": norm, "version": version, "default_class": default_class,
                    "restaled": restaled}


# ---- item bindings (item_set head / mf / ruling / gate_class) --------------------------

def split_binding(fields: dict) -> tuple[dict, dict]:
    gate = {k: fields.pop(k) for k in BINDING_FIELDS if k in fields}
    return fields, gate


def _binding_values(gate: dict) -> dict:
    out = {}
    if "head" in gate:
        out["head"] = gr._sha(gate["head"] or None, "head")
    if "mf" in gate:
        out["mf"] = gr._sha(gate["mf"] or None, "mf")
    if "ruling" in gate:
        out["ruling_id"] = gr._str(gate["ruling"] or None, "ruling", 80)
    if "gate_class" in gate:
        value = gate["gate_class"] or None
        if value is not None and not gr.GATE_NAME_RE.match(value):
            raise gr.bad("gate_class must be lowercase [a-z0-9_.-]", "gate_class")
        out["gate_class"] = value
    return out


async def apply_binding(conn, tx: BoardTx, ns: list[int], gate: dict) -> None:
    """Within an item write: move the bindings and restale what they invalidate."""
    values = _binding_values(gate)
    board = tx.board
    for n in ns:
        it = tx.item(n)
        row = await _item_row(conn, board, n)
        binding = dict(row["gate_binding"] or {})
        prev = dict(binding)
        changed = set()
        for key in ("head", "mf", "ruling_id"):
            if key in values and binding.get(key) != values[key]:
                changed.add(key)
                if values[key] is None:
                    binding.pop(key, None)
                else:
                    binding[key] = values[key]
        class_changed = "gate_class" in values and row["gate_class"] != values["gate_class"]
        if not changed and not class_changed:
            continue
        await conn.execute("UPDATE items SET gate_binding = $2, gate_class = $3 WHERE id = $1",
                           row["id"], binding,
                           values["gate_class"] if class_changed else row["gate_class"])
        tx._event(it, "gate_binding", prev={**prev, "gate_class": row["gate_class"]},
                  new={**binding, "gate_class": values.get("gate_class", row["gate_class"])})
        live = await _receipts(conn, board, "r.item_id = $2 AND r.stale_at IS NULL", row["id"])
        for rec in live:
            why = gr.binding_stale(rec, binding, changed)
            if why is None and class_changed:
                why = {"reason": "class_changed", "prev": row["gate_class"] or "(default)",
                       "new": values["gate_class"] or "(default)"}
            if why:
                await _mark_stale(conn, tx, board, [rec], why)


# ---- refresh (§8): the posting host's observations ----------------------------------

async def refresh(pool, daemon_id: str, checks: list[dict]) -> dict:
    now = items_db.utcnow()
    stale: list[str] = []
    async with pool.acquire() as conn:
        by_board: dict[str, list] = {}
        if checks:
            ids = [c.get("receipt_id") for c in checks if isinstance(c.get("receipt_id"), int)]
            rows = await conn.fetch(
                """SELECT id, board_id FROM item_receipts
                    WHERE id = ANY($1::bigint[]) AND poster_daemon_id = $2""", ids, daemon_id)
            owner = {r["id"]: str(r["board_id"]) for r in rows}
            for c in checks:
                bid = owner.get(c.get("receipt_id"))
                if bid:
                    by_board.setdefault(bid, []).append(c)
        for bid in sorted(by_board):
            async with conn.transaction():
                board, items = await _locked(conn, bid)
                tx = BoardTx(board, items, {"pid": "system:gate-refresh", "name": "gate refresh"}, now)
                for c in by_board[bid]:
                    found = await _receipts(conn, board, "r.id = $2", c["receipt_id"])
                    if not found:
                        continue
                    rec = found[0]
                    await conn.execute("UPDATE item_receipts SET checked_at = $2 WHERE id = $1",
                                       rec["id"], now)
                    if rec["stale_at"] is not None or c.get("error"):
                        continue
                    why = gr.refresh_stale(rec, profile_for(board, rec["class"]), _clean_check(c))
                    if why:
                        stale += await _mark_stale(conn, tx, board, [rec], why)
                await items_db._apply(conn, tx)
        watch = await conn.fetch(
            """SELECT DISTINCT ON (r.item_id, r.gate) r.id, r.head_sha, r.branch, r.base,
                      r.runner, r.worktree, r.class, r.stale_at, b.gate_profiles
                 FROM item_receipts r JOIN items i ON i.id = r.item_id
                 JOIN boards b ON b.id = r.board_id
                WHERE r.poster_daemon_id = $1 AND i.closed_at IS NULL AND i.archived_at IS NULL
                ORDER BY r.item_id, r.gate, r.id DESC""", daemon_id)
    out = []
    for w in watch:
        if w["stale_at"] is not None or not (w["worktree"] or {}).get("path"):
            continue
        prof = (w["gate_profiles"] or {}).get(w["class"]) or {}
        base = w["base"] or {}
        out.append({
            "receipt_id": w["id"], "path": w["worktree"]["path"], "branch": w["branch"],
            "head": w["head_sha"], "base_ref": base.get("ref") or (prof.get("base") or {}).get("ref"),
            "base_sha_after": base.get("sha_after"),
            "runner_ref": (w["runner"] or {}).get("ref"),
            "scope": (prof.get("base") or {}).get("scope", "paths"),
        })
        if len(out) >= MAX_WATCH:
            break
    return {"stale": stale, "watch": out, "server_time": iso(now)}


def _clean_check(c: dict) -> dict:
    out = {}
    for key in ("branch_head", "runner_sha", "base_sha"):
        value = c.get(key)
        if isinstance(value, str) and gr.SHA_RE.match(value.lower()):
            out[key] = value.lower()
    tree = c.get("merge_tree")
    if isinstance(tree, str):
        out["merge_tree"] = tree.lower()
    paths = c.get("changed_paths")
    if isinstance(paths, list):
        out["changed_paths"] = [p for p in paths if isinstance(p, str)][:5000]
    out["changed_paths_truncated"] = bool(c.get("changed_paths_truncated"))
    return out
