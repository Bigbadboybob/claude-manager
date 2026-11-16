"""Bounded, exact-number board reads. No board-wide item load is performed."""

from __future__ import annotations

import uuid

from dispatch.items_db import _board_ref, holder_state, utcnow
from dispatch.items_rules import ItemsError, iso

# Characters, not bytes. JSON escaping and UTF-8 can expand them; all fields
# and row counts in one response are bounded independently.
CHUNK = 2048
PREVIEW = 240
PREVIEW_ROWS = 5
MAX_POSITION = 2**31 - 2
SECTIONS = frozenset({"summary", "text", "record", "links", "holders", "history",
                      "flags", "blocked_by", "blocks"})
TEXT_FIELDS = {
    "title": "COALESCE(t.name, i.title)",
    "description": "CASE WHEN t.name IS NOT NULL THEN t.description ELSE i.description END",
    "note": "i.note", "group": "i.grp", "blocked_on": "i.blocked_on", "job": "i.job",
}


def _bad(message):
    return ItemsError(422, "invalid_field", message)


def _page(chunk, offset, *, index=None, next_index=None):
    # SQL produces at most CHUNK + 1 characters. Explicit offsets make every
    # omitted character retrievable, including large JSON history records.
    if chunk is None:
        return {"data": None, "next_offset": None, "next_index": next_index, "index": index}
    more = len(chunk) > CHUNK
    return {"data": chunk[:CHUNK], "next_offset": offset + CHUNK if more else None,
            "next_index": next_index, "index": index}


async def read(pool, ref, n, *, task_id=None, section="summary", field=None, index=0, offset=0):
    if isinstance(n, bool) or not isinstance(n, int) or not 1 <= n <= 2**31 - 1:
        raise _bad("n must be an item number between 1 and 2147483647")
    if section not in SECTIONS:
        raise _bad("unknown item section")
    if section == "text" and field not in TEXT_FIELDS:
        raise _bad("text field must be title, description, note, group, blocked_on or job")
    if section != "text" and field is not None:
        raise _bad("field is only valid for the text section")
    for name, value in (("index", index), ("offset", offset)):
        if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= MAX_POSITION:
            raise _bad(f"{name} is out of range")
    if section == "summary" and (index or offset):
        raise _bad("summary does not accept an index or offset")
    if section in ("text", "record", "links") and index:
        raise _bad("index applies only to list sections")

    async with pool.acquire() as conn:
        async with conn.transaction(isolation="repeatable_read", readonly=True):
            if bool(ref) == bool(task_id):
                raise _bad("pass exactly one board or caller task")
            if ref:
                column, value = _board_ref(ref)
                if column == "id":
                    value = uuid.UUID(value)
                board = await conn.fetchrow(
                    f"SELECT id, left(slug,120) AS slug, length(slug)>120 AS slug_truncated "
                    f"FROM boards WHERE {column}=$1", value)
            else:
                try:
                    task_uuid = uuid.UUID(task_id)
                except (ValueError, TypeError):
                    raise _bad("caller task must be a UUID") from None
                chain = await conn.fetch("""
                    WITH RECURSIVE chain AS (
                        SELECT id,parent_task_id,initiative_id,0 AS depth FROM tasks WHERE id=$1
                        UNION ALL
                        SELECT t.id,t.parent_task_id,t.initiative_id,c.depth+1
                          FROM tasks t JOIN chain c ON t.id=c.parent_task_id WHERE c.depth<64
                    ) SELECT * FROM chain ORDER BY depth
                """, task_uuid)
                if not chain:
                    raise ItemsError(404, "not_found", "caller task not found")
                initiative = next((r["initiative_id"] for r in chain if r["initiative_id"]), None)
                column, value = ("initiative_id", initiative) if initiative else ("root_task_id", chain[-1]["id"])
                board = await conn.fetchrow(
                    f"SELECT id, left(slug,120) AS slug, length(slug)>120 AS slug_truncated "
                    f"FROM boards WHERE {column}=$1", value)
            if board is None:
                raise ItemsError(404, "not_found", "board not found")
            # Read a single indexed item, including closed and archived items.
            row = await conn.fetchrow("""
                SELECT i.id, i.number, i.status, i.task_id, i.group_id, i.team_id,
                       i.link_archived_at, i.description_rev, i.check_back_at, i.eta_at,
                       i.waiting_set_at, i.actionable_at, i.launched_at, i.touched_at,
                       i.closed_at, i.archived_at,
                       left(COALESCE(t.name, i.title), $3) AS title,
                       length(COALESCE(t.name, i.title)) > $3 AS title_truncated,
                       left(CASE WHEN t.name IS NOT NULL THEN t.description ELSE i.description END, $3) AS description,
                       length(CASE WHEN t.name IS NOT NULL THEN t.description ELSE i.description END) > $3 AS description_truncated,
                       left(i.note, $3) AS note, length(i.note) > $3 AS note_truncated,
                       left(i.grp, $3) AS grp, length(i.grp) > $3 AS group_truncated,
                       left(i.blocked_on, $3) AS blocked_on, length(i.blocked_on) > $3 AS blocked_on_truncated,
                       left(i.job, $3) AS job, length(i.job) > $3 AS job_truncated,
                       CASE WHEN t.name IS NOT NULL THEN t.description_rev ELSE i.description_rev END AS effective_description_rev
                  FROM items i LEFT JOIN tasks t ON t.id=i.task_id
                 WHERE i.board_id=$1 AND i.number=$2
            """, board["id"], n, PREVIEW)
            if row is None:
                raise ItemsError(404, "not_found", f"no item #{n} on board {board['slug']}", n=n)
            item_id = row["id"]
            base = {"board": board["slug"], "board_id": str(board["id"]),
                    "board_slug_truncated": board["slug_truncated"], "n": n, "section": section,
                    "paging": "Each call is a separate snapshot; refresh if the item changes."}
            if section == "summary":
                holders = await conn.fetch("""
                    SELECT left(h.pid,300) AS pid, left(h.name,200) AS name, left(h.role,40) AS role,
                           s.pid AS state_pid, s.state, s.state_since, s.reported_at, s.exited_at, s.reported_done
                      FROM item_holders h LEFT JOIN session_states s ON s.pid=h.pid
                     WHERE h.item_id=$1 ORDER BY h.added_at,h.pid LIMIT $2
                """, item_id, PREVIEW_ROWS + 1)
                flags = await conn.fetch("""
                    SELECT left(kind,80) AS kind, raised_at FROM item_flags
                     WHERE item_id=$1 AND resolved_at IS NULL ORDER BY raised_at,id LIMIT $2
                """, item_id, PREVIEW_ROWS + 1)
                blockers = await conn.fetch("""
                    SELECT x.number FROM item_deps d JOIN items x ON x.id=d.blocker_id
                     WHERE d.item_id=$1 ORDER BY x.number LIMIT $2
                """, item_id, PREVIEW_ROWS + 1)
                blocked = await conn.fetch("""
                    SELECT x.number FROM item_deps d JOIN items x ON x.id=d.item_id
                     WHERE d.blocker_id=$1 AND x.status NOT IN ('done','dropped')
                     ORDER BY x.number LIMIT $2
                """, item_id, PREVIEW_ROWS + 1)
                now = utcnow()
                item = {k: iso(row[k]) for k in ("number", "status", "title", "description", "note", "blocked_on",
                         "job", "check_back_at", "eta_at", "waiting_set_at", "actionable_at", "launched_at",
                         "touched_at", "closed_at", "archived_at", "link_archived_at")}
                item["n"] = item.pop("number")
                item["group"] = row["grp"]
                for key in ("task_id", "group_id", "team_id"):
                    item[key] = str(row[key]) if row[key] else None
                item["description_rev"] = row["effective_description_rev"]
                item["truncated"] = {key: bool(row[key + "_truncated"])
                                     for key in ("title", "description", "note", "group", "blocked_on", "job")}
                item["holders"] = [
                    {"pid": h["pid"], "name": h["name"], "role": h["role"],
                     "state": holder_state(h if h["state_pid"] is not None else None, now)}
                    for h in holders[:PREVIEW_ROWS]]
                item["holders_more"] = len(holders) > PREVIEW_ROWS
                item["holders_are_previews"] = True
                item["flags"] = [{"kind": f["kind"], "raised_at": iso(f["raised_at"])}
                                 for f in flags[:PREVIEW_ROWS]]
                item["flags_more"] = len(flags) > PREVIEW_ROWS
                item["flags_are_previews"] = True
                item["blocked_by"] = [r["number"] for r in blockers[:PREVIEW_ROWS]]
                item["blocked_by_more"] = len(blockers) > PREVIEW_ROWS
                item["blocks"] = [r["number"] for r in blocked[:PREVIEW_ROWS]]
                item["blocks_more"] = len(blocked) > PREVIEW_ROWS
                item["not_loaded"] = ["record", "links", "history"]
                item["detail_sections"] = ["text", "record", "links", "holders", "history", "flags",
                                           "blocked_by", "blocks"]
                return {**base, "item": item}

            if section in ("text", "record", "links"):
                expr = TEXT_FIELDS[field] if section == "text" else (
                    "to_jsonb(i)::text" if section == "record" else "i.links::text")
                value = await conn.fetchval(
                    f"SELECT substring(({expr}) from $2::int for $3::int) FROM items i "
                    "LEFT JOIN tasks t ON t.id=i.task_id WHERE i.id=$1",
                    item_id, offset + 1, CHUNK + 1)
                return {**base, "field": field, **_page(value, offset)}

            # One list record per request, itself chunked as JSON text; this
            # permits retrieval even when attribution or history JSON is large.
            sources = {
                "holders": ("item_holders x", "x.item_id=$1", "x.added_at,x.pid", "to_jsonb(x)::text"),
                "history": ("item_events x", "x.item_id=$1", "x.id DESC", "to_jsonb(x)::text"),
                "flags": ("item_flags x", "x.item_id=$1 AND x.resolved_at IS NULL", "x.raised_at,x.id", "to_jsonb(x)::text"),
                "blocked_by": ("item_deps d JOIN items x ON x.id=d.blocker_id", "d.item_id=$1", "x.number", "jsonb_build_object('n',x.number,'status',x.status)::text"),
                "blocks": ("item_deps d JOIN items x ON x.id=d.item_id", "d.blocker_id=$1 AND x.status NOT IN ('done','dropped')", "x.number", "jsonb_build_object('n',x.number,'status',x.status)::text"),
            }
            source, where, order, expression = sources[section]
            # All interpolated SQL is a closed, server-owned allowlist.
            record = await conn.fetchrow(
                f"SELECT substring(({expression}) from $2::int for $3::int) AS chunk FROM {source} "
                f"WHERE {where} ORDER BY {order} LIMIT 1 OFFSET $4",
                item_id, offset + 1, CHUNK + 1, index)
            if record is None:
                if index == 0 and offset == 0:
                    return {**base, **_page(None, offset, index=index)}
                raise ItemsError(404, "not_found", f"no {section} record at index {index}")
            exists = await conn.fetchval(
                f"SELECT EXISTS(SELECT 1 FROM {source} WHERE {where} ORDER BY {order} OFFSET $2 LIMIT 1)",
                item_id, index + 1)
            return {**base, **_page(record["chunk"], offset, index=index,
                                    next_index=index + 1 if exists else None)}
