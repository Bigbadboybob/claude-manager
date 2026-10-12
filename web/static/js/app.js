// CM Board app: conversation list, conversation view, composer, alerts,
// availability and notification settings. Plain DOM, no framework.
import { api, allPages, newRequestId, ApiError } from "./api.js";
import {
  REACTIONS, LEVELS, renderBody, formatTime, dayLabel, sortConversations, filterConversations,
  badgeCount, reactionList, truncate, parseRoute, routeHash, groupAlerts, age, withoutSeenAlerts,
} from "./format.js";
import * as push from "./push.js";

const PAGE = 50;
const REFRESH_MS = 30_000;

const state = {
  me: null,
  channels: [],
  dms: [],
  people: new Map(), // id -> name
  counts: { conversations: {}, unread: 0, dms: 0, mentions: 0 },
  availability: null,
  alerts: [],
  // Alert ids acknowledged here: hidden at once and kept hidden even if a
  // refresh that raced the acknowledgement still returns them.
  seenAlerts: new Map(), // id -> time acknowledged
  route: parseRoute(location.hash),
  query: "",
  conv: null, // {id, items, next, loadingOlder, replyTo, thread, atBottom}
  authExpired: false,
  busy: new Set(),
};

// ---------- tiny DOM helper ----------
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "html") el.innerHTML = v;
    else if (k === "class") el.className = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/** replaceChildren without the null/false placeholders conditionals produce
 *  (the DOM would render them as the text "null"). */
function put(el, ...children) {
  el.replaceChildren(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
}

function toast(message, error = false) {
  document.querySelector(".toast")?.remove();
  const t = h("div", { class: `toast${error ? " error" : ""}`, role: "status" }, message);
  document.body.append(t);
  setTimeout(() => t.remove(), error ? 6000 : 2500);
}

function fail(e) {
  if (e instanceof ApiError && e.authExpired) {
    state.authExpired = true;
    render();
    return;
  }
  toast(e.message || String(e), true);
}

// ---------- drafts (survive errors, reloads and Access expiry) ----------
const draftKey = (id) => `cm-board:draft:${id}`;
function loadDraft(id) {
  try {
    return JSON.parse(localStorage.getItem(draftKey(id)) || "null") || { body: "", request_id: null, reply_to: null };
  } catch {
    return { body: "", request_id: null, reply_to: null };
  }
}
function saveDraft(id, draft) {
  try {
    if (!draft.body && !draft.request_id) localStorage.removeItem(draftKey(id));
    else localStorage.setItem(draftKey(id), JSON.stringify(draft));
  } catch {
    // Storage unavailable (private mode): drafts last for the page only.
  }
}

// ---------- directory ----------
function personName(id) {
  if (id === "owner") return "Owner";
  return state.people.get(id) || String(id).split(":").pop();
}

function convCounts(id) {
  return state.counts.conversations?.[id] || {};
}

function conversationRows() {
  const channels = state.channels
    .filter((c) => c.joined !== false)
    .map((c) => ({
      id: c.id,
      kind: "channel",
      name: `#${c.name || c.path}`,
      topic: c.path,
      unread: convCounts(c.id).unread ?? c.unread ?? 0,
      mentions: convCounts(c.id).mentions ?? c.mentions ?? 0,
    }));
  const dms = state.dms.map((d) => ({
    id: d.id,
    kind: "dm",
    name: dmName(d),
    unread: convCounts(d.id).unread ?? d.unread ?? 0,
    mentions: convCounts(d.id).mentions ?? d.mentions ?? 0,
  }));
  return { channels, dms };
}

function dmName(d) {
  const members = (d.members || d.peers || (d.peer ? [d.peer] : [])).filter((m) => m !== "owner");
  return members.length ? members.map(personName).join(", ") : "DM";
}

function conversationTitle(id) {
  const c = state.channels.find((x) => x.id === id);
  if (c) return `#${c.name || c.path}`;
  const d = state.dms.find((x) => x.id === id);
  if (d) return dmName(d);
  return "Conversation";
}

async function loadDirectory() {
  const [channels, dms, people] = await Promise.all([
    allPages((c) => api.channels(c)),
    allPages((c) => api.dms(c)),
    allPages((c) => api.people(c)),
  ]);
  state.channels = channels;
  state.dms = dms;
  state.people = new Map(people.map((p) => [p.id, p.name]));
}

async function loadCounts() {
  state.counts = await api.counts();
  push.setBadge(badgeCount(state.counts));
}

// ---------- conversation ----------
function newestAtBottom(items) {
  return [...items].reverse();
}

function mergeItems(existing, incoming) {
  const byId = new Map(existing.map((m) => [m.id, m]));
  for (const m of incoming) byId.set(m.id, m);
  return [...byId.values()].sort((a, b) => {
    const t = String(a.created_at).localeCompare(String(b.created_at));
    return t || String(a.id).localeCompare(String(b.id));
  });
}

async function openConversation(id, messageId = null) {
  state.conv = { id, items: [], next: null, loadingOlder: false, replyTo: null, thread: null, atBottom: true, flash: messageId };
  render();
  try {
    // A direct newest-page read marks the conversation read (API default).
    const page = await api.read({ conversation: id, newest_first: true, limit: PAGE });
    if (state.conv?.id !== id) return;
    state.conv.items = newestAtBottom(page.items || []);
    state.conv.next = page.next_cursor || null;
    // A push opened a specific message: page back until it is loaded.
    for (let i = 0; messageId && i < 10 && state.conv.next && !state.conv.items.some((m) => m.id === messageId); i += 1) {
      await loadOlder(false);
    }
    render();
    scrollConversation(messageId);
    await loadCounts();
    renderList();
  } catch (e) {
    fail(e);
  }
}

async function loadOlder(rerender = true) {
  const c = state.conv;
  if (!c || !c.next || c.loadingOlder) return;
  c.loadingOlder = true;
  try {
    const page = await api.read({ conversation: c.id, newest_first: true, limit: PAGE, cursor: c.next, mark_read: false });
    if (state.conv !== c) return;
    const scroller = document.querySelector(".conv .scroll");
    const before = scroller ? scroller.scrollHeight - scroller.scrollTop : 0;
    c.items = mergeItems(c.items, page.items || []);
    c.next = page.next_cursor || null;
    if (rerender) {
      renderConversation();
      // Keep the reader's place while history grows above.
      const s = document.querySelector(".conv .scroll");
      if (s) s.scrollTop = s.scrollHeight - before;
    }
  } catch (e) {
    fail(e);
  } finally {
    c.loadingOlder = false;
  }
}

async function refreshConversation() {
  const c = state.conv;
  if (!c) return;
  try {
    // Only mark read while the newest message is on screen.
    const page = await api.read({ conversation: c.id, newest_first: true, limit: PAGE, mark_read: c.atBottom });
    if (state.conv !== c) return;
    const had = c.items.length;
    c.items = mergeItems(c.items, page.items || []);
    if (c.items.length !== had || (page.items || []).length) {
      const stick = c.atBottom;
      renderConversation();
      if (stick) scrollConversation();
    }
  } catch (e) {
    fail(e);
  }
}

function scrollConversation(messageId = null) {
  const s = document.querySelector(".conv .scroll");
  if (!s) return;
  if (messageId) {
    const el = s.querySelector(`[data-id="${CSS.escape(messageId)}"]`);
    if (el) {
      el.scrollIntoView({ block: "center" });
      el.classList.add("flash");
      setTimeout(() => el.classList.remove("flash"), 2500);
      return;
    }
  }
  s.scrollTop = s.scrollHeight;
}

function mentionsOwner(m) {
  return m.data?.mention_here === true || (m.data?.mention_recipients || []).includes("owner");
}

async function send() {
  const c = state.conv;
  if (!c) return;
  const box = document.querySelector(".composer textarea");
  const draft = loadDraft(c.id);
  draft.body = box ? box.value : draft.body;
  if (!draft.body.trim()) return;
  if (draft.body.length > 3000) {
    toast("Over 3000 characters", true);
    return;
  }
  // Keep the request id until the send is confirmed: a retry after a
  // timeout must not post twice.
  draft.request_id = draft.request_id || newRequestId();
  draft.reply_to = c.replyTo?.id || draft.reply_to || null;
  saveDraft(c.id, draft);
  state.busy.add("send");
  renderComposer();
  try {
    await sendDraft(c.id, draft);
  } catch (e) {
    if (e instanceof ApiError && e.code === "join_required") {
      if (confirm("Join this channel to post?")) {
        try {
          await api.join(c.id, newRequestId());
          await sendDraft(c.id, draft);
        } catch (e2) {
          fail(e2);
        }
      }
    } else {
      fail(e);
    }
  } finally {
    state.busy.delete("send");
    renderComposer();
  }
}

async function sendDraft(id, draft) {
  const params = { conversation: id, body: draft.body, request_id: draft.request_id };
  if (draft.reply_to) params.reply_to = draft.reply_to;
  await api.send(params);
  saveDraft(id, { body: "", request_id: null, reply_to: null });
  if (state.conv?.id === id) {
    state.conv.replyTo = null;
    state.conv.atBottom = true;
  }
  const box = document.querySelector(".composer textarea");
  if (box) box.value = "";
  await refreshConversation();
  scrollConversation();
}

async function react(message, emoji) {
  const mine = reactionList(message.reactions).find((r) => r.emoji === emoji)?.mine || false;
  try {
    const res = await api.react(message.id, emoji, mine, newRequestId());
    message.reactions = res.reactions || message.reactions;
    if (res.counts) {
      state.counts = res.counts;
      push.setBadge(badgeCount(state.counts));
    }
    renderConversation(true);
  } catch (e) {
    fail(e);
  }
}

async function markRead(params, label) {
  try {
    const res = await api.markRead(params);
    if (res.counts) state.counts = res.counts;
    else await loadCounts();
    push.setBadge(badgeCount(state.counts));
    toast(label);
    render();
  } catch (e) {
    fail(e);
  }
}

async function openThread(root) {
  const c = state.conv;
  if (!c) return;
  c.thread = { root, items: [] };
  renderThread();
  try {
    const page = await api.read({ conversation: c.id, thread: root.id, newest_first: true, limit: 100, mark_read: false });
    if (state.conv?.thread?.root.id !== root.id) return;
    c.thread.items = newestAtBottom(page.items || []);
    renderThread();
  } catch (e) {
    fail(e);
  }
}

// ---------- alerts / availability / settings ----------
async function loadAlerts() {
  try {
    const res = await api.alerts();
    state.alerts = visibleAlerts(res.items || []);
  } catch (e) {
    fail(e);
  }
}

/** Drop alerts acknowledged here; forget an acknowledgement once the server
 *  stops listing that alert (or after two minutes, in case it never did). */
function visibleAlerts(items) {
  return withoutSeenAlerts(items, state.seenAlerts, Date.now());
}

async function setLevel(level) {
  try {
    const res = await api.setAvailability(level, newRequestId());
    state.availability = res.owner_availability || state.availability;
    toast(`Availability: ${level}`);
    closeSheet();
    renderHeader();
  } catch (e) {
    fail(e);
  }
}

// ---------- rendering ----------
const root = () => document.getElementById("app");

function render() {
  const app = root();
  put(app);
  if (state.authExpired) {
    app.append(
      h("div", { class: "bar" }, h("h1", {}, "CM Board")),
      h("div", { class: "empty" },
        h("p", {}, "Your sign-in has expired. Drafts are kept."),
        h("button", { class: "primary", onclick: () => location.reload() }, "Sign in again")),
    );
    return;
  }
  app.append(h("header", { id: "header" }), h("main", { id: "main" }));
  renderHeader();
  renderMain();
}

function renderHeader() {
  const el = document.getElementById("header");
  if (!el) return;
  const level = state.availability?.level || "unset";
  const route = state.route;
  const inConv = route.view === "conversation";
  const title = inConv ? conversationTitle(route.id) : route.view === "alerts" ? "Alerts" : "CM Board";
  put(el, h("div", { class: "bar" },
    inConv || route.view === "alerts"
      ? h("button", { class: "ghost back", "aria-label": "Back", onclick: () => go({ view: "list" }) }, "‹")
      : null,
    h("h1", {}, title),
    h("button", { class: "pill", "data-level": level, "aria-label": "Availability", onclick: availabilitySheet },
      h("span", { class: "dot" }), level),
    h("button", { class: "ghost", "aria-label": "Alerts", onclick: () => go({ view: "alerts" }) },
      "⚑", state.alerts.length ? h("span", { class: "badge at" }, groupAlerts(state.alerts).length) : null),
    h("button", { class: "ghost", "aria-label": "Settings", onclick: settingsSheet }, "⚙"),
  ));
}

function renderMain() {
  const main = document.getElementById("main");
  if (!main) return;
  const view = state.route.view;
  const listPane = h("section", { class: `list-pane pane${view === "list" ? "" : " hidden-narrow"}`, id: "list" });
  put(main, listPane);
  renderList();
  if (view === "conversation") {
    main.append(h("section", { class: "pane conv", id: "conv" }));
    renderConversation();
  } else if (view === "alerts") {
    main.append(h("section", { class: "pane", id: "alerts" }));
    renderAlerts();
  } else {
    main.append(h("section", { class: "pane hidden-narrow" },
      h("div", { class: "empty" }, "Choose a conversation")));
  }
}

function renderList() {
  const el = document.getElementById("list");
  if (!el) return;
  const search = h("div", { class: "search" }, h("input", {
    type: "search", placeholder: "Filter conversations", value: state.query, "aria-label": "Filter",
    // Only the rows re-render, so the input keeps focus while typing.
    oninput: (e) => { state.query = e.target.value; renderListRows(); },
  }));
  put(el, search, h("div", { class: "scroll", id: "list-rows" }));
  renderListRows();
}

function renderListRows() {
  const rows = document.getElementById("list-rows");
  if (!rows) return;
  const { channels, dms } = conversationRows();
  const all = sortConversations(filterConversations([...channels, ...dms], state.query));
  const attention = all.filter((r) => r.mentions > 0 || (r.kind === "dm" && r.unread > 0));
  const rest = all.filter((r) => !attention.includes(r));
  const row = (r) => h("button", {
    class: `row${r.unread ? " unread" : ""}${state.route.id === r.id ? " active" : ""}`,
    onclick: () => go({ view: "conversation", id: r.id }),
  },
  h("span", { class: "name" }, r.name),
  r.mentions ? h("span", { class: "badge at" }, `@${r.mentions}`) : null,
  r.unread && !r.mentions ? h("span", { class: "badge" }, r.unread) : null);
  const alertRows = groupAlerts(state.alerts);
  const alertsRow = alertRows.length && !state.query ? h("button", {
    class: "row unread", onclick: () => go({ view: "alerts" }),
  }, h("span", { class: "name" }, `⚑ Open alerts · ${alertRows.map((r) => r.label).join(", ")}`),
  h("span", { class: "badge at" }, alertRows.length)) : null;
  put(rows,
    attention.length || alertsRow ? h("div", { class: "section" }, "Needs you") : null,
    alertsRow,
    ...attention.map(row),
    rest.length ? h("div", { class: "section" }, "Conversations") : null,
    ...rest.map(row),
    !attention.length && !rest.length ? h("div", { class: "empty" }, state.query ? "No match" : "Loading…") : null,
  );
}

function messageEl(m, { inThread = false } = {}) {
  const mine = m.actor?.id === "owner";
  const counts = reactionList(m.reactions);
  const replyTo = m.data?.reply_to ? state.conv?.items.find((x) => x.id === m.data.reply_to) : null;
  return h("article", {
    class: `msg${mine ? " mine" : ""}${mentionsOwner(m) ? " mention" : ""}`, "data-id": m.id,
    // Tap a message to show its reactions and reply actions.
    onclick: (e) => {
      if (e.target.closest("button, a")) return;
      const el = e.currentTarget;
      const open = !el.classList.contains("open");
      el.closest(".messages, .sheet")?.querySelectorAll(".msg.open").forEach((x) => x.classList.remove("open"));
      el.classList.toggle("open", open);
    },
  },
  h("div", { class: "meta" },
    h("span", { class: "who" }, m.actor?.name || personName(m.actor?.id)),
    h("time", { datetime: m.created_at }, formatTime(m.created_at))),
  replyTo && !inThread
    ? h("div", { class: "thread" }, `↳ ${replyTo.actor?.name || ""}: ${truncate(replyTo.body, 80)}`)
    : null,
  m.type && m.type !== "message.create" && !m.body
    ? h("div", { class: "body" }, h("em", {}, m.type))
    : h("div", { class: "body", html: renderBody(m.body) }),
  counts.length ? h("div", { class: "reactions" }, ...counts.map((r) => h("button", {
    class: `reaction${r.mine ? " mine" : ""}`, title: r.names.join(", "), onclick: () => react(m, r.emoji),
  }, `${r.emoji} ${r.count}`))) : null,
  h("div", { class: "actions" },
    ...REACTIONS.map((e) => h("button", { class: "ghost", "aria-label": `React ${e}`, onclick: () => react(m, e) }, e)),
    inThread ? null : h("button", { class: "ghost", onclick: () => startReply(m) }, "Reply"),
    inThread ? null : h("button", { class: "ghost", onclick: () => openThread(m) }, "Thread")));
}

function renderConversation(keepScroll = false) {
  const el = document.getElementById("conv");
  const c = state.conv;
  if (!el || !c) return;
  const prev = el.querySelector(".scroll");
  const prevTop = prev?.scrollTop;
  const counts = convCounts(c.id);
  const tools = h("div", { class: "bar toolbar" },
    h("span", { class: "sub" }, c.items.length ? `${c.items.length} loaded` : ""),
    h("span", { class: "spacer" }),
    counts.mentions
      ? h("button", { onclick: () => markRead({ conversation: c.id, mentions: true }, "Mentions cleared") }, `Clear @${counts.mentions}`)
      : null,
    h("button", { onclick: () => markRead({ conversation: c.id }, "Marked read") }, "Mark read"));
  const list = h("div", { class: "messages" });
  if (c.next) list.append(h("button", { class: "older", onclick: () => loadOlder() }, "Load older"));
  let lastDay = "";
  for (const m of c.items) {
    const day = dayLabel(m.created_at);
    if (day !== lastDay) {
      list.append(h("div", { class: "day" }, day));
      lastDay = day;
    }
    list.append(messageEl(m));
  }
  if (!c.items.length) list.append(h("div", { class: "empty" }, "No messages yet"));
  const scroller = h("div", { class: "scroll", onscroll: onConversationScroll }, list);
  put(el, tools, scroller, h("div", { id: "composer" }));
  renderComposer();
  if (keepScroll && prevTop !== undefined) scroller.scrollTop = prevTop;
}

function onConversationScroll(e) {
  const s = e.target;
  if (!state.conv) return;
  state.conv.atBottom = s.scrollHeight - s.scrollTop - s.clientHeight < 40;
  if (s.scrollTop < 200 && state.conv.next && !state.conv.loadingOlder) loadOlder();
  // Reaching the newest message marks the conversation read (once).
  if (state.conv.atBottom && convCounts(state.conv.id).unread && !state.conv.marking) {
    const c = state.conv;
    c.marking = true;
    refreshConversation().then(loadCounts).then(renderListRows).catch(fail).finally(() => { c.marking = false; });
  }
}

function startReply(m) {
  if (!state.conv) return;
  state.conv.replyTo = m;
  const draft = loadDraft(state.conv.id);
  draft.reply_to = m.id;
  saveDraft(state.conv.id, draft);
  renderComposer();
  document.querySelector(".composer textarea")?.focus();
}

function renderComposer() {
  const el = document.getElementById("composer");
  const c = state.conv;
  if (!el || !c) return;
  const draft = loadDraft(c.id);
  const replyTo = c.replyTo || (draft.reply_to ? c.items.find((m) => m.id === draft.reply_to) : null);
  const box = h("textarea", {
    rows: 1, placeholder: `Message ${conversationTitle(c.id)}`, "aria-label": "Message",
    oninput: (e) => {
      const d = loadDraft(c.id);
      d.body = e.target.value;
      saveDraft(c.id, d);
      autosize(e.target);
    },
    onkeydown: (e) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        send();
      }
    },
  });
  box.value = draft.body || "";
  put(el, 
    replyTo || draft.reply_to ? h("div", { class: "replying" },
      `Replying to ${replyTo ? `${replyTo.actor?.name}: ${truncate(replyTo.body, 60)}` : "a message"}`,
      h("button", { class: "ghost", "aria-label": "Cancel reply", onclick: () => {
        c.replyTo = null;
        const d = loadDraft(c.id);
        d.reply_to = null;
        saveDraft(c.id, d);
        renderComposer();
      } }, "✕")) : null,
    h("div", { class: "composer" }, box,
      h("button", { class: "primary", disabled: state.busy.has("send"), onclick: send },
        state.busy.has("send") ? "…" : draft.request_id ? "Retry" : "Send")),
  );
  autosize(box);
}

function autosize(t) {
  t.style.height = "auto";
  t.style.height = `${Math.min(t.scrollHeight, window.innerHeight * 0.4)}px`;
}

function renderThread() {
  const t = state.conv?.thread;
  if (!t) return;
  openSheet(`Thread`, [
    messageEl(t.root, { inThread: true }),
    ...t.items.filter((m) => m.id !== t.root.id).map((m) => messageEl(m, { inThread: true })),
    h("button", { class: "primary wide gap-top", onclick: () => {
      closeSheet();
      startReply(t.root);
    } }, "Reply in thread"),
  ], () => { if (state.conv) state.conv.thread = null; });
}

function renderAlerts() {
  const el = document.getElementById("alerts");
  if (!el) return;
  // One row per agent with an open notify_user alert, oldest first.
  const rows = groupAlerts(state.alerts);
  const seen = (row) => async (e) => {
    e.target.disabled = true;
    // Hide by id first: a background refresh may replace state.alerts (new
    // objects, possibly fetched before the acknowledgement committed).
    const ids = row.alerts.map((a) => a.id);
    const now = Date.now();
    for (const id of ids) state.seenAlerts.set(id, now);
    const redraw = () => {
      state.alerts = state.alerts.filter((a) => !state.seenAlerts.has(a.id));
      renderAlerts();
      renderHeader();
      renderListRows();
    };
    redraw();
    try {
      for (const a of row.alerts) await api.alertSeen(a.key, a.id);
    } catch (err) {
      // Not acknowledged: show the row again.
      for (const id of ids) state.seenAlerts.delete(id);
      fail(err);
      await loadAlerts();
    }
    redraw();
  };
  put(el, h("div", { class: "scroll" },
    // v1 lists the hub host's alerts only; cm-sessions alerts arrive with the
    // hub relay (B5b). Hidden once the server reports more hosts.
    state.me?.features?.alert_hosts?.length > 1 ? null
      : h("div", { class: "section" }, "Alerts from cm-manager only (cm-sessions alerts coming)"),
    rows.length ? rows.map((row) => h("div", {
      class: "alert", id: `alert-row-${row.key}`, "data-alerts": row.alerts.map((a) => a.id).join(" "),
    },
    h("div", { class: "meta" },
      h("strong", {}, row.label),
      row.host ? h("span", { class: "sub" }, ` · ${row.host}`) : null,
      h("span", { class: "spacer" }),
      row.count > 1 ? h("span", { class: "badge at" }, row.count) : null,
      h("span", { class: "when" }, ` ${age(row.since)}`)),
    h("div", { class: "body" }, row.reason),
    h("button", { onclick: seen(row) }, row.count > 1 ? `Seen (${row.count})` : "Seen")))
      : h("div", { class: "empty" }, "No open alerts")));
  if (state.route.alert) {
    [...el.querySelectorAll(".alert")].find((x) => x.dataset.alerts.split(" ").includes(state.route.alert))
      ?.scrollIntoView({ block: "center" });
  }
}

function openSheet(title, children, onClose) {
  closeSheet();
  const back = h("div", { class: "sheet-back", onclick: (e) => { if (e.target === back) closeSheet(); } },
    h("div", { class: "sheet", role: "dialog", "aria-label": title }, h("h2", {}, title), ...children));
  back.onCloseSheet = onClose;
  document.body.append(back);
}

function closeSheet() {
  const back = document.querySelector(".sheet-back");
  if (!back) return;
  back.remove();
  back.onCloseSheet?.();
}

function availabilitySheet() {
  const current = state.availability?.level || "unset";
  const what = {
    away: "Emergencies only",
    around: "Decisions and above",
    focused: "Everything",
    "on-call": "Everything",
  };
  openSheet("Availability", [
    ...LEVELS.map((l) => h("button", { class: `opt${l === current ? " on" : ""}`, onclick: () => setLevel(l) },
      h("span", {}, l), h("span", { class: "sub" }, what[l]))),
    h("p", {}, "Push notifications follow this level, as on the desktop (F7)."),
  ]);
}

async function settingsSheet() {
  const status = await push.pushState().catch(() => "unsupported");
  const lines = {
    "install-first": "To get notifications on iPhone: tap Share, then Add to Home Screen, and open CM Board from the Home Screen.",
    unsupported: "This browser does not support web notifications.",
    denied: "Notifications are blocked. Allow them in Settings → Notifications → CM Board.",
    off: "Notifications are off on this device.",
    on: "Notifications are on for this device.",
  };
  const busy = (btn, fn) => async () => {
    btn.disabled = true;
    try {
      await fn();
    } catch (e) {
      fail(e);
    } finally {
      btn.disabled = false;
    }
  };
  const children = [h("p", {}, lines[status])];
  if (status === "off") {
    const b = h("button", { class: "primary wide" }, "Enable notifications");
    b.onclick = busy(b, async () => {
      await push.enable();
      toast("Notifications enabled");
      settingsSheet();
    });
    children.push(b);
  }
  if (status === "on") {
    const test = h("button", { class: "wide gap-bottom" }, "Send test notification");
    test.onclick = busy(test, async () => {
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.getSubscription();
      await api.pushTest(sub?.endpoint);
      toast("Test sent — it should arrive in a few seconds");
    });
    const off = h("button", { class: "wide" }, "Turn off on this device");
    off.onclick = busy(off, async () => {
      await push.disable();
      toast("Notifications off");
      settingsSheet();
    });
    children.push(test, off);
  }
  children.push(
    h("button", { class: "opt", onclick: () => markRead({ all: true }, "Everything marked read") }, "Mark everything read"),
    h("p", {}, state.me?.user?.email ? `Signed in as ${state.me.user.email}` : ""),
  );
  openSheet("Settings", children);
}

// ---------- routing & refresh ----------
function go(route) {
  const hash = routeHash(route);
  if (location.hash === hash || (hash === "#" && !location.hash)) onRoute();
  else location.hash = hash;
}

function onRoute() {
  const route = parseRoute(location.hash);
  const prev = state.route;
  state.route = route;
  closeSheet();
  render();
  if (route.view === "conversation" && (prev.view !== "conversation" || prev.id !== route.id || route.message || !state.conv)) {
    openConversation(route.id, route.message);
  } else if (route.view === "conversation") {
    renderConversation();
  } else {
    state.conv = null;
    if (route.view === "alerts") loadAlerts().then(() => { renderAlerts(); renderHeader(); });
  }
}

let refreshTimer = null;
let refreshing = false;
async function refresh() {
  if (refreshing || document.visibilityState !== "visible" || state.authExpired) return;
  refreshing = true;
  try {
    await Promise.all([loadCounts(), loadDirectory(), loadAlerts(), loadAvailability()]);
    renderList();
    renderHeader();
    if (state.conv) await refreshConversation();
  } catch (e) {
    fail(e);
  } finally {
    refreshing = false;
  }
}

async function loadAvailability() {
  const res = await api.availability();
  state.availability = res.owner_availability || null;
}

function scheduleRefresh() {
  clearInterval(refreshTimer);
  if (document.visibilityState === "visible") refreshTimer = setInterval(refresh, REFRESH_MS);
}

async function start() {
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
    navigator.serviceWorker.addEventListener("message", (e) => {
      if (e.data?.type === "navigate" && e.data.url) {
        const url = new URL(e.data.url, location.origin);
        location.hash = url.hash || "#";
      }
    });
  }
  window.addEventListener("hashchange", onRoute);
  document.addEventListener("visibilitychange", () => {
    scheduleRefresh();
    if (document.visibilityState === "visible") refresh();
  });
  render();
  try {
    state.me = await api.bootstrap();
    state.availability = state.me.owner_availability || null;
    await Promise.all([loadDirectory(), loadCounts(), loadAlerts()]);
  } catch (e) {
    fail(e);
  }
  onRoute();
  scheduleRefresh();
}

start();
