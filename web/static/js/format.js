// Pure helpers (no DOM): unit-tested under Node by web/dev/smoke.mjs.

export const REACTIONS = ["✅", "👀", "👍", "❌", "🎉"];
export const LEVELS = ["away", "around", "focused", "on-call"];

/** HTML-escape text for innerHTML use. */
export function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

/** Escape, then render `code` spans and bare http(s) links. Nothing else is
 *  interpreted, so message bodies can never inject markup. */
export function renderBody(text) {
  const escaped = escapeHtml(text);
  return escaped
    .replace(/`([^`\n]+)`/g, "<code>$1</code>")
    .replace(/\bhttps?:\/\/[^\s<]+[^\s<.,;:!?)\]'"]/g,
      (url) => `<a href="${url}" target="_blank" rel="noopener noreferrer">${url}</a>`);
}

/** `14:05`, `Yesterday 14:05`, `Mon 14:05` or `3 Oct 14:05` relative to now. */
export function formatTime(iso, now = new Date()) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const hm = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
  const day = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diffDays = Math.round((day(now) - day(d)) / 86400000);
  if (diffDays === 0) return hm;
  if (diffDays === 1) return `Yesterday ${hm}`;
  if (diffDays > 1 && diffDays < 7) return `${d.toLocaleDateString([], { weekday: "short" })} ${hm}`;
  return `${d.getDate()} ${d.toLocaleDateString([], { month: "short" })} ${hm}`;
}

/** Day separator label for the timeline (`Today`, `Yesterday`, `Mon 6 Oct`). */
export function dayLabel(iso, now = new Date()) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const day = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diffDays = Math.round((day(now) - day(d)) / 86400000);
  if (diffDays === 0) return "Today";
  if (diffDays === 1) return "Yesterday";
  return d.toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" });
}

/** Sort conversations for the list: mentions first, then unread, then the
 *  rest by name. Rows: {name, mentions, unread}. Stable for ties. */
export function sortConversations(rows) {
  const rank = (r) => ((r.mentions || 0) > 0 ? 0 : (r.unread || 0) > 0 ? 1 : 2);
  return [...rows].sort((a, b) =>
    rank(a) - rank(b) || String(a.name).localeCompare(String(b.name)));
}

/** Case-insensitive substring filter over name (and topic when present). */
export function filterConversations(rows, query) {
  const q = String(query || "").trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((r) => `${r.name} ${r.topic || ""}`.toLowerCase().includes(q));
}

/** App-badge number: the server's deduplicated `unread` (DMs plus channel
 *  mentions), never a sum of per-channel unread counts. */
export function badgeCount(counts) {
  return counts?.unread || 0;
}

/** Reactions as [{emoji, count, names, mine}], allowlist order first. Accepts
 *  {emoji: {count, names, mine}} or {emoji: [names]}. */
export function reactionList(reactions) {
  if (!reactions || typeof reactions !== "object") return [];
  const out = Object.entries(reactions).map(([emoji, r]) => {
    const names = Array.isArray(r) ? r : (r?.names || []);
    const count = Array.isArray(r) ? r.length : (r?.count ?? names.length);
    const mine = Array.isArray(r) ? names.includes("Owner") : !!r?.mine;
    return { emoji, count, names, mine };
  }).filter((r) => r.count > 0);
  const order = (e) => { const i = REACTIONS.indexOf(e); return i < 0 ? REACTIONS.length : i; };
  return out.sort((a, b) => order(a.emoji) - order(b.emoji));
}

/** Truncate to `max` characters with an ellipsis. */
export function truncate(s, max) {
  const t = String(s ?? "");
  return t.length <= max ? t : `${t.slice(0, Math.max(0, max - 1))}…`;
}

/** VAPID public key (base64url) → Uint8Array for pushManager.subscribe. */
export function urlBase64ToUint8Array(base64) {
  const padded = (base64 + "===".slice((base64.length + 3) % 4)).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(padded);
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

/** Parse the app route from location.hash, in the push URLs' format
 *  (web/API.md): `#conversation=<id>&message=<id>`, `#alerts&alert=<id>`,
 *  `#settings`, or the list. */
export function parseRoute(hash) {
  const raw = String(hash || "").replace(/^#\/?/, "");
  const params = new URLSearchParams(raw);
  if (params.get("conversation")) {
    return { view: "conversation", id: params.get("conversation"), message: params.get("message") || null };
  }
  if (params.has("alerts")) return { view: "alerts", alert: params.get("alert") || null };
  if (params.has("settings")) return { view: "settings" };
  return { view: "list" };
}

export function routeHash(route) {
  if (route.view === "conversation") {
    const p = new URLSearchParams({ conversation: route.id });
    if (route.message) p.set("message", route.message);
    return `#${p.toString()}`;
  }
  if (route.view === "alerts") return route.alert ? `#alerts&alert=${encodeURIComponent(route.alert)}` : "#alerts";
  if (route.view === "settings") return "#settings";
  return "#";
}

/** Compact age: `45s`, `12m`, `3h`, `2d`. */
export function age(iso, now = new Date()) {
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.round((now.getTime() - t) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** Open Owner alerts, one row per agent (session + host), oldest first:
 *  {key, label, host, reason (first line of the newest), since (oldest),
 *  count, alerts}. */
export function groupAlerts(alerts) {
  // Original creation time when the relay supplies it (B5b), else the
  // held/released stamps of older daemons.
  const when = (a) => a.created_at || a.held_since || a.released_at || "";
  const rows = new Map();
  for (const a of alerts || []) {
    // origin_host is the relay's stable host id; display names can collide.
    const key = `${a.origin_host || a.host || ""}|${a.session_uid || a.label || a.id}`;
    const row = rows.get(key) || { key, label: a.label || a.session_uid || "agent", host: a.host || null, alerts: [] };
    row.alerts.push(a);
    rows.set(key, row);
  }
  return [...rows.values()].map((r) => {
    r.alerts.sort((x, y) => when(x).localeCompare(when(y)));
    const newest = r.alerts[r.alerts.length - 1];
    return {
      ...r,
      count: r.alerts.length,
      since: when(r.alerts[0]),
      reason: String(newest.message || "").split("\n")[0],
    };
  }).sort((x, y) => x.since.localeCompare(y.since));
}

/** Alerts minus those acknowledged locally (`seen`: id -> ms). An entry is
 *  forgotten once the server stops listing that alert, or after `ttlMs` in
 *  case it never does; until then a refresh that raced the acknowledgement
 *  cannot bring the row back. Mutates `seen` (pruning). */
export function withoutSeenAlerts(items, seen, nowMs, ttlMs = 120_000) {
  const listed = new Set((items || []).map((a) => a.id));
  for (const [id, at] of seen) {
    if (!listed.has(id) || nowMs - at > ttlMs) seen.delete(id);
  }
  return (items || []).filter((a) => !seen.has(a.id));
}

/** Remember display names seen in DM rows (`member_names`, the last
 *  message's actor) and in loaded messages, so departed agents (absent from
 *  the people directory) keep their names. Mutates and returns `known`. */
export function learnNames(known, { dms = [], messages = [] } = {}) {
  for (const d of dms) {
    for (const [id, name] of Object.entries(d.member_names || {})) if (name) known.set(id, name);
    const a = d.last?.actor;
    if (a?.id && a.name) known.set(a.id, a.name);
  }
  for (const m of messages) {
    if (m.actor?.id && m.actor.name) known.set(m.actor.id, m.actor.name);
  }
  return known;
}

/** Display name: Owner, the people directory, names seen elsewhere, and
 *  only then the bare id's last segment. */
export function nameFor(id, people, known) {
  if (id === "owner") return "Owner";
  return people?.get(id) || known?.get(id) || String(id).split(":").pop();
}
