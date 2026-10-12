// Web Push subscription. iOS (16.4+) allows push only for a Home Screen app
// and only from a user gesture, so `enable()` must run inside a tap handler.
import { api } from "./api.js";
import { urlBase64ToUint8Array } from "./format.js";

export function isStandalone() {
  return window.matchMedia("(display-mode: standalone)").matches || window.navigator.standalone === true;
}

export function isIos() {
  return /iPad|iPhone|iPod/.test(navigator.userAgent)
    || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
}

/** What the settings sheet should say/offer: one of
 *  unsupported | install-first | denied | off | on. */
export async function pushState() {
  if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) {
    return isIos() && !isStandalone() ? "install-first" : "unsupported";
  }
  if (Notification.permission === "denied") return "denied";
  const reg = await navigator.serviceWorker.ready;
  const sub = await reg.pushManager.getSubscription();
  return sub ? "on" : "off";
}

/** Ask permission, subscribe with the server's VAPID key, register the
 *  subscription. Call from a click handler. */
export async function enable() {
  const permission = await Notification.requestPermission();
  if (permission !== "granted") throw new Error("Notifications were not allowed");
  const reg = await navigator.serviceWorker.ready;
  const { public_key: key, enabled } = await api.vapidPublicKey();
  if (enabled === false || !key) throw new Error("Push is not configured on the server");
  let sub = await reg.pushManager.getSubscription();
  if (!sub) {
    sub = await reg.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: urlBase64ToUint8Array(key),
    });
  }
  await api.pushSubscribe(sub.toJSON());
  return sub;
}

export async function disable() {
  const reg = await navigator.serviceWorker.ready;
  const sub = await reg.pushManager.getSubscription();
  if (!sub) return;
  try {
    await api.pushUnsubscribe(sub.endpoint);
  } finally {
    await sub.unsubscribe();
  }
}

/** App icon badge (Home Screen apps). Best effort. */
export function setBadge(n) {
  try {
    if (!("setAppBadge" in navigator)) return;
    if (n > 0) navigator.setAppBadge(n);
    else navigator.clearAppBadge();
  } catch {
    // Not available on this platform.
  }
}
