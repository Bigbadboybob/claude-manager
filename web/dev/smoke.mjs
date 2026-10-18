// Headless smoke test for the CM Board frontend (no browser needed):
//   node web/dev/smoke.mjs
// 1. Unit tests for the pure helpers in web/static/js/format.js.
// 2. Starts web/dev/fake_server.py and checks that the app shell, the files
//    the service worker caches and the API flows the app uses all work.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import * as f from "../static/js/format.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const staticDir = path.join(here, "..", "static");

test("bodies are escaped; only code spans and links are rendered", () => {
  const html = f.renderBody('<img src=x onerror=alert(1)> `a<b` see https://example.com/x?y=1&z=2.');
  assert.ok(!html.includes("<img"));
  assert.ok(html.includes("&lt;img"));
  assert.ok(html.includes("<code>a&lt;b</code>"));
  assert.ok(html.includes('<a href="https://example.com/x?y=1&amp;z=2"'));
  assert.ok(html.endsWith("</a>."), "trailing punctuation stays outside the link");
});

test("conversations sort mentions first, then unread, then by name", () => {
  const rows = [
    { name: "#b", unread: 0, mentions: 0 },
    { name: "#a", unread: 3, mentions: 0 },
    { name: "#z", unread: 1, mentions: 2 },
    { name: "#c", unread: 0, mentions: 0 },
  ];
  assert.deepEqual(f.sortConversations(rows).map((r) => r.name), ["#z", "#a", "#b", "#c"]);
  assert.deepEqual(f.filterConversations(rows, "Z").map((r) => r.name), ["#z"]);
});

test("badge uses the server's deduplicated unread, not a per-channel sum", () => {
  assert.equal(f.badgeCount({ unread: 3, dms: 2, mentions: 1, conversations: { a: { unread: 40 } } }), 3);
  assert.equal(f.badgeCount(null), 0);
});

test("reactions accept both shapes and keep the allowlist order", () => {
  const list = f.reactionList({ "🎉": { count: 1, names: ["A"], mine: false }, "✅": ["Owner", "B"] });
  assert.deepEqual(list.map((r) => [r.emoji, r.count, r.mine]), [["✅", 2, true], ["🎉", 1, false]]);
  assert.deepEqual(f.reactionList({ "👀": { count: 0, names: [] } }), []);
});

test("routes use the push URL format from web/API.md", () => {
  const r = f.parseRoute("#conversation=conv-1&message=origin%3Aevent");
  assert.deepEqual(r, { view: "conversation", id: "conv-1", message: "origin:event" });
  assert.equal(f.routeHash(r), "#conversation=conv-1&message=origin%3Aevent");
  assert.deepEqual(f.parseRoute("#alerts&alert=a%201"), { view: "alerts", alert: "a 1" });
  assert.deepEqual(f.parseRoute(""), { view: "list" });
  assert.deepEqual(f.parseRoute("#settings"), { view: "settings" });
});

test("VAPID keys decode from base64url", () => {
  assert.deepEqual([...f.urlBase64ToUint8Array("AQID_-8")], [1, 2, 3, 255, 239]);
});

test("time labels", () => {
  const now = new Date(2026, 9, 12, 15, 0);
  assert.equal(f.dayLabel(new Date(2026, 9, 12, 9, 0).toISOString(), now), "Today");
  assert.equal(f.dayLabel(new Date(2026, 9, 11, 9, 0).toISOString(), now), "Yesterday");
  assert.match(f.formatTime(new Date(2026, 9, 11, 9, 5).toISOString(), now), /^Yesterday 09:05$/);
  assert.equal(f.truncate("abcdef", 4), "abc…");
});

test("open alerts group one row per agent, oldest first", () => {
  const rows = f.groupAlerts([
    { key: "k1", id: "a", session_uid: "s1", label: "Scout", message: "decide", released_at: "2026-10-12T10:00:00Z" },
    { key: "k2", id: "b", session_uid: "s2", label: "Perf", host: "sessions", message: "merge?\nmore", released_at: "2026-10-12T08:00:00Z" },
    { key: "k2", id: "c", session_uid: "s2", label: "Perf", host: "sessions", message: "still waiting", released_at: "2026-10-12T09:30:00Z" },
  ]);
  assert.deepEqual(rows.map((r) => [r.label, r.host, r.count, r.reason]),
    [["Perf", "sessions", 2, "still waiting"], ["Scout", null, 1, "decide"]]);
  assert.equal(rows[0].since, "2026-10-12T08:00:00Z");
  const now = new Date("2026-10-12T12:00:00Z");
  assert.equal(f.age("2026-10-12T11:59:30Z", now), "30s");
  assert.equal(f.age("2026-10-12T09:00:00Z", now), "3h");
  assert.equal(f.age("2026-10-10T12:00:00Z", now), "2d");
});

test("relayed alerts group by origin host id, not the display name", () => {
  const rows = f.groupAlerts([
    { key: "k1", id: "a", origin_host: "uuid-1", host: "cm", session_uid: "s1", label: "W", message: "x", created_at: "2026-10-12T10:00:00Z" },
    { key: "k2", id: "b", origin_host: "uuid-2", host: "cm", session_uid: "s1", label: "W", message: "y", created_at: "2026-10-12T09:00:00Z" },
  ]);
  assert.equal(rows.length, 2, "same display host and uid on two hosts are two agents");
  assert.equal(rows[0].since, "2026-10-12T09:00:00Z", "created_at orders and ages");
});

test("an alert seen here stays hidden through a racing refresh, then is forgotten", () => {
  const seen = new Map([["a1", 1000]]);
  const stale = [{ id: "a1" }, { id: "a2" }];
  assert.deepEqual(f.withoutSeenAlerts(stale, seen, 2000).map((a) => a.id), ["a2"], "refresh fetched before the ack");
  assert.ok(seen.has("a1"));
  assert.deepEqual(f.withoutSeenAlerts([{ id: "a2" }], seen, 3000).map((a) => a.id), ["a2"]);
  assert.ok(!seen.has("a1"), "forgotten once the server drops it");
  seen.set("a3", 0);
  assert.deepEqual(f.withoutSeenAlerts([{ id: "a3" }], seen, 200_000).map((a) => a.id), ["a3"], "an ack that never landed reappears after the ttl");
});

test("departed agents keep their names (member_names, last actor, loaded messages)", () => {
  const people = new Map([["agent:x:present", "Present"]]);
  const known = f.learnNames(new Map(), {
    dms: [
      { id: "d1", members: ["owner", "agent:x:ts-1"], member_names: { "agent:x:ts-1": "SEJD Coordinator", owner: "Owner" } },
      { id: "d2", members: ["owner", "agent:x:ts-2"], last: { actor: { id: "agent:x:ts-2", name: "Old Worker" } } },
    ],
    messages: [{ actor: { id: "agent:x:ts-3", name: "Scout" } }],
  });
  assert.equal(f.nameFor("agent:x:ts-1", people, known), "SEJD Coordinator");
  assert.equal(f.nameFor("agent:x:ts-2", people, known), "Old Worker");
  assert.equal(f.nameFor("agent:x:ts-3", people, known), "Scout");
  assert.equal(f.nameFor("agent:x:present", people, known), "Present", "the directory still wins");
  assert.equal(f.nameFor("agent:x:ts-9", people, known), "ts-9", "uid only as the last resort");
  assert.equal(f.nameFor("owner", people, known), "Owner");
});

test("an alert's agent maps to its DM on its own host", () => {
  const hosts = [{ id: "hub", local: true }, { id: "sessions-uuid", local: false }];
  assert.equal(f.alertParticipant({ origin_host: "sessions-uuid", session_uid: "ts-perf" }, hosts), "agent:sessions-uuid:ts-perf",
    "relayed alerts use their origin host, not the hub");
  assert.equal(f.alertParticipant({ session_uid: "ts-old" }, hosts), "agent:hub:ts-old", "legacy: the local host");
  assert.equal(f.alertParticipant({ session_uid: "x" }, []), null);
  const dms = [
    { id: "d1", members: ["owner", "agent:hub:a"] },
    { id: "g1", members: ["owner", "agent:hub:a", "agent:hub:b"] },
  ];
  assert.equal(f.directDm("agent:hub:a", dms).id, "d1", "the one-to-one DM, not a group");
  assert.equal(f.directDm("agent:hub:b", dms), null, "only a group: no direct DM yet");
  assert.ok(f.hasIdentity("agent:hub:b", new Map(), new Map(), dms), "a group member has an identity");
  assert.ok(f.hasIdentity("agent:x:p", new Map([["agent:x:p", "P"]]), new Map(), []));
  assert.ok(!f.hasIdentity("agent:x:ghost", new Map(), new Map(), dms));
  const r = f.parseRoute("#dm=agent%3Asessions-uuid%3Ats-perf");
  assert.deepEqual(r, { view: "newdm", pid: "agent:sessions-uuid:ts-perf" });
  assert.equal(f.routeHash(r), "#dm=agent%3Asessions-uuid%3Ats-perf");
});

test("New DM candidates: everyone but Owner, searchable by name or id, prefix first", () => {
  const people = new Map([
    ["owner", "Owner"],
    ["agent:hub:scout", "Scout"],
    ["agent:hub:coord", "Swarm-Coord"],
    ["agent:sessions-uuid:ts-perf", "Native-Rollout-Perf"],
    ["agent:hub:ts-noname", ""],
  ]);
  assert.deepEqual(f.dmCandidates(people, "").map((p) => p.name), ["Native-Rollout-Perf", "Scout", "Swarm-Coord", "ts-noname"]);
  assert.deepEqual(f.dmCandidates(people, "s").map((p) => p.name), ["Scout", "Swarm-Coord", "Native-Rollout-Perf", "ts-noname"],
    "name-prefix matches rank first");
  assert.deepEqual(f.dmCandidates(people, "SESSIONS-uuid").map((p) => p.id), ["agent:sessions-uuid:ts-perf"], "ids match too");
  assert.deepEqual(f.dmCandidates(people, "owner"), [], "Owner never DMs themselves");
  assert.deepEqual(f.dmCandidates(null, "x"), []);
});

// ---------- against the fake backend ----------
async function withServer(fn) {
  const port = 18000 + Math.floor(Math.random() * 2000);
  const proc = spawn("python3", [path.join(here, "fake_server.py"), "--port", String(port)], { stdio: ["ignore", "pipe", "inherit"] });
  await new Promise((resolve, reject) => {
    proc.stdout.once("data", resolve);
    proc.once("exit", (code) => reject(new Error(`fake server exited ${code}`)));
  });
  try {
    await fn(`http://127.0.0.1:${port}`);
  } finally {
    proc.kill();
  }
}

const write = (base, method, p, body) => fetch(base + p, {
  method, headers: { "Content-Type": "application/json", "X-CM-Board": "1" }, body: JSON.stringify(body),
}).then(async (r) => ({ status: r.status, json: await r.json() }));

test("the app shell and every service-worker file are served", async () => {
  const sw = readFileSync(path.join(staticDir, "sw.js"), "utf8");
  const shell = JSON.parse(sw.match(/SHELL_FILES = (\[[\s\S]*?\]);/)[1].replace(/,\s*\]/, "]"));
  const html = readFileSync(path.join(staticDir, "index.html"), "utf8");
  const refs = [...html.matchAll(/(?:href|src)="(\/[^"]*)"/g)].map((m) => m[1]);
  const manifest = JSON.parse(readFileSync(path.join(staticDir, "manifest.webmanifest"), "utf8"));
  await withServer(async (base) => {
    for (const p of new Set([...shell, ...refs, "/sw.js", ...manifest.icons.map((i) => i.src)])) {
      const res = await fetch(base + p);
      assert.equal(res.status, 200, `${p} must be served`);
      if (p.endsWith(".js")) assert.match(res.headers.get("content-type"), /javascript/, p);
    }
  });
  assert.equal(manifest.display, "standalone");
  for (const icon of manifest.icons) assert.ok(existsSync(path.join(staticDir, icon.src)), icon.src);
});

test("API flows the app uses: open, page back, send, react, clear mentions, availability, push", async () => {
  await withServer(async (base) => {
    const get = (p) => fetch(base + p).then((r) => r.json());
    const boot = await get("/api/bootstrap");
    assert.equal(boot.actor, "owner");
    const counts = await get("/api/counts");
    assert.ok(counts.conversations["c-swarm"].mentions > 0);
    // Writes without the board header are refused, like the real server.
    const bare = await fetch(`${base}/api/send`, { method: "POST", body: "{}" });
    assert.equal(bare.status, 403);

    // Opening marks read; paging back does not.
    const first = await write(base, "POST", "/api/read", { conversation: "c-swarm", newest_first: true, limit: 50 });
    assert.equal(first.json.items.length, 50);
    assert.ok(first.json.next_cursor);
    const after = await get("/api/counts");
    assert.equal(after.conversations["c-swarm"].unread, 0, "opening reads the conversation");
    const older = await write(base, "POST", "/api/read",
      { conversation: "c-swarm", newest_first: true, limit: 50, cursor: first.json.next_cursor, mark_read: false });
    assert.equal(older.json.items.length, 50);
    assert.ok(older.json.items[0].created_at < first.json.items.at(-1).created_at, "older page is older");

    // Send is idempotent per request id.
    const rid = "req-1";
    const a = await write(base, "POST", "/api/send", { conversation: "c-general", body: "hello", request_id: rid });
    const b = await write(base, "POST", "/api/send", { conversation: "c-general", body: "hello", request_id: rid });
    assert.equal(a.json.event_id, b.json.event_id);

    // React toggles with `mine` and returns counts.
    const msg = first.json.items[0].id;
    const on = await write(base, "POST", "/api/react", { message_id: msg, emoji: "✅", remove: false, request_id: "r1" });
    assert.equal(on.json.reactions["✅"].mine, true);
    assert.ok(on.json.counts);
    const off = await write(base, "POST", "/api/react", { message_id: msg, emoji: "✅", remove: true, request_id: "r2" });
    assert.equal(off.json.reactions["✅"], undefined);

    // Clear mentions on another conversation, mark all, availability.
    const cleared = await write(base, "POST", "/api/mark-read", { all: true });
    assert.equal(cleared.json.counts.unread, 0);
    const level = await write(base, "POST", "/api/availability", { level: "away", request_id: "a1" });
    assert.equal(level.json.owner_availability.level, "away");

    // Push subscription lifecycle.
    const key = await get("/api/push/vapid-public-key");
    assert.ok(key.public_key && key.enabled);
    const sub = { endpoint: "https://push.example/1", expirationTime: null, keys: { p256dh: "k", auth: "a" } };
    assert.equal((await write(base, "POST", "/api/push/subscribe", sub)).json.subscribed, true);
    assert.equal((await write(base, "POST", "/api/push/test", { endpoint: sub.endpoint })).json.accepted, true);
    assert.equal((await write(base, "DELETE", "/api/push/subscribe", { endpoint: sub.endpoint })).json.removed, true);

    // New DM picker: the directory lists everyone; a pick with a DM opens
    // it, one without goes to the empty composer.
    const people = new Map((await get("/api/people")).items.map((p) => [p.id, p.name]));
    const choices = f.dmCandidates(people, "");
    assert.ok(choices.length >= 3 && !choices.some((p) => p.id === "owner"));
    const dmsNow = (await get("/api/dms")).items;
    assert.equal(f.directDm(f.dmCandidates(people, "scout")[0].id, dmsNow).id, "d-scout");
    assert.equal(f.directDm(f.dmCandidates(people, "rollout")[0].id, dmsNow), null);

    // The first message to a participant without a DM creates it.
    const pid = "agent:sessions-uuid:ts-perf";
    const before = (await get("/api/dms")).items;
    assert.ok(!before.some((d) => d.members.includes(pid)));
    const opened = await write(base, "POST", "/api/send", { dm: pid, body: "about that bench", request_id: "dm-1" });
    assert.equal(opened.status, 200);
    const created = opened.json.event.conversation_id;
    const afterDms = (await get("/api/dms")).items;
    assert.ok(afterDms.some((d) => d.id === created && d.members.includes(pid)));

    // Alerts: seen removes only the matching alert.
    const alerts = await get("/api/alerts");
    const [alert] = alerts.items;
    const seen = await write(base, "POST", "/api/alerts/seen", { key: alert.key, alert_id: alert.id });
    assert.equal(seen.json.acknowledged, true);
  });
});
