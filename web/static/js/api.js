// HTTP client for web/API.md. Same-origin only; writes carry the JSON
// content type and `X-CM-Board: 1`. Errors are thrown as ApiError with the
// server's {error:{code,message}} (403 = Access session expired / origin).

export class ApiError extends Error {
  constructor(status, code, message) {
    super(message || code || `HTTP ${status}`);
    this.status = status;
    this.code = code || String(status);
  }
  get authExpired() {
    return this.status === 403 || this.status === 401;
  }
}

async function request(method, path, { query, body } = {}) {
  let url = path;
  if (query) {
    const params = new URLSearchParams();
    for (const [k, v] of Object.entries(query)) {
      if (v === undefined || v === null || v === "") continue;
      params.set(k, typeof v === "object" ? JSON.stringify(v) : String(v));
    }
    const qs = params.toString();
    if (qs) url += `?${qs}`;
  }
  const init = { method, credentials: "same-origin", headers: { Accept: "application/json" } };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.headers["X-CM-Board"] = "1";
    init.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(url, init);
  } catch (e) {
    throw new ApiError(0, "network", "Network unavailable");
  }
  // An expired Access session answers with a redirect to the login page,
  // which fetch follows into HTML: treat any non-JSON reply as expiry.
  const type = res.headers.get("content-type") || "";
  if (res.redirected || !type.includes("application/json")) {
    if (res.ok || res.redirected || res.status === 403) {
      throw new ApiError(403, "access", "Sign-in expired");
    }
    throw new ApiError(res.status, String(res.status), res.statusText);
  }
  const data = await res.json();
  if (!res.ok) {
    throw new ApiError(res.status, data?.error?.code, data?.error?.message);
  }
  return data;
}

export const newRequestId = () =>
  (crypto.randomUUID ? crypto.randomUUID() : `${Date.now()}-${Math.random().toString(16).slice(2)}`);

export const api = {
  bootstrap: () => request("GET", "/api/bootstrap"),
  channels: (cursor) => request("GET", "/api/channels", { query: { cursor, limit: 200 } }),
  dms: (cursor) => request("GET", "/api/dms", { query: { cursor, limit: 200 } }),
  people: (cursor) => request("GET", "/api/people", { query: { cursor, limit: 200 } }),
  counts: () => request("GET", "/api/counts"),
  read: (params) => request("POST", "/api/read", { body: params }),
  send: (params) => request("POST", "/api/send", { body: params }),
  join: (conversation, requestId) =>
    request("POST", "/api/channels/join", { body: { conversation, request_id: requestId } }),
  markRead: (params) => request("POST", "/api/mark-read", { body: params }),
  react: (messageId, emoji, remove, requestId) =>
    request("POST", "/api/react", { body: { message_id: messageId, emoji, remove, request_id: requestId } }),
  alerts: () => request("GET", "/api/alerts"),
  alertSeen: (key, alertId) => request("POST", "/api/alerts/seen", { body: { key, alert_id: alertId } }),
  availability: () => request("GET", "/api/availability"),
  setAvailability: (level, requestId) =>
    request("POST", "/api/availability", { body: { level, request_id: requestId } }),
  vapidPublicKey: () => request("GET", "/api/push/vapid-public-key"),
  pushSubscribe: (subscription) => request("POST", "/api/push/subscribe", { body: subscription }),
  pushUnsubscribe: (endpoint) => request("DELETE", "/api/push/subscribe", { body: { endpoint } }),
  pushTest: (endpoint) => request("POST", "/api/push/test", { body: endpoint ? { endpoint } : {} }),
};

/** Fetch every page of a cursor-paged directory (channels, DMs, people). */
export async function allPages(fetchPage) {
  const items = [];
  let cursor;
  for (let i = 0; i < 50; i += 1) {
    const page = await fetchPage(cursor);
    items.push(...(page.items || []));
    if (!page.next_cursor) break;
    cursor = page.next_cursor;
  }
  return items;
}
