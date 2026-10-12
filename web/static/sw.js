// CM Board service worker: app-shell cache, Web Push display, tap routing.
// API responses are never cached (they are live and behind Access).
const SHELL = "cm-board-shell-v1";
const SHELL_FILES = [
  "/",
  "/index.html",
  "/app.css",
  "/manifest.webmanifest",
  "/js/app.js",
  "/js/api.js",
  "/js/format.js",
  "/js/push.js",
  "/icons/icon-192.png",
  "/icons/apple-touch-icon.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(SHELL).then((c) => c.addAll(SHELL_FILES)).then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== SHELL).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

// Network first for the shell (so a deploy shows up), cache as the offline
// fallback. Everything under /api/ goes straight to the network.
self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.origin !== self.location.origin || url.pathname.startsWith("/api/")) {
    return;
  }
  event.respondWith(
    fetch(event.request)
      .then((res) => {
        if (res.ok && SHELL_FILES.includes(url.pathname)) {
          const copy = res.clone();
          caches.open(SHELL).then((c) => c.put(event.request, copy));
        }
        return res;
      })
      .catch(() => caches.match(event.request).then((hit) => hit || caches.match("/"))),
  );
});

// Payload: {title, body, url, tag, badge}. iOS shows nothing for a push that
// does not display a notification, so always show one.
self.addEventListener("push", (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch {
    data = { body: event.data ? event.data.text() : "" };
  }
  const title = data.title || "CM Board";
  const options = {
    body: data.body || "",
    tag: data.tag || undefined,
    data: { url: data.url || "/" },
    icon: "/icons/icon-192.png",
    badge: "/icons/icon-192.png",
  };
  const tasks = [self.registration.showNotification(title, options)];
  if (typeof data.badge === "number" && self.navigator && "setAppBadge" in self.navigator) {
    tasks.push(data.badge > 0 ? self.navigator.setAppBadge(data.badge) : self.navigator.clearAppBadge());
  }
  event.waitUntil(Promise.all(tasks).catch(() => {}));
});

// A tap focuses an open window and routes it, or opens the app at the URL.
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = (event.notification.data && event.notification.data.url) || "/";
  event.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((wins) => {
      for (const w of wins) {
        if ("focus" in w) {
          w.postMessage({ type: "navigate", url: target });
          return w.focus();
        }
      }
      return self.clients.openWindow(target);
    }),
  );
});
