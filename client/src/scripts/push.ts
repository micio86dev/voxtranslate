//! Web Push opt-in for the consumer app (spec: scheduled meetings, Phase 1e).
//! Registers the existing service worker for push and subscribes via the server's
//! VAPID key. Best-effort and non-throwing; the call flow never depends on it.

import { authHeaders, HTTP_BASE, isLoggedIn } from "./auth";

function urlBase64ToUint8Array(base64: string): Uint8Array {
  const padding = "=".repeat((4 - (base64.length % 4)) % 4);
  const b64 = (base64 + padding).replace(/-/g, "+").replace(/_/g, "/");
  const raw = atob(b64);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

function supported(): boolean {
  return (
    "serviceWorker" in navigator &&
    "PushManager" in window &&
    "Notification" in window
  );
}

async function vapidKey(): Promise<string | null> {
  try {
    const res = await fetch(`${HTTP_BASE}/api/push/vapid-public-key`);
    if (!res.ok) return null;
    return ((await res.json()) as { key?: string }).key || null;
  } catch {
    return null;
  }
}

/**
 * Subscribe this browser to push if the user is signed in and has already granted
 * notification permission. Silent — never prompts. Call `enablePush()` from a user
 * gesture to prompt. Safe to call on every load.
 */
export async function maybeSubscribePush(): Promise<void> {
  if (!supported() || !isLoggedIn() || Notification.permission !== "granted")
    return;
  await subscribe();
}

/** Prompt for permission and subscribe. Returns true on success. */
export async function enablePush(): Promise<boolean> {
  if (!supported() || !isLoggedIn()) return false;
  const perm = await Notification.requestPermission();
  if (perm !== "granted") return false;
  return subscribe();
}

async function subscribe(): Promise<boolean> {
  try {
    await navigator.serviceWorker.register("/sw.js");
    const reg = await navigator.serviceWorker.ready;
    const key = await vapidKey();
    if (!key) return false;
    const sub = await reg.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: urlBase64ToUint8Array(key) as BufferSource,
    });
    const json = sub.toJSON() as {
      endpoint?: string;
      keys?: { p256dh?: string; auth?: string };
    };
    if (!json.endpoint || !json.keys?.p256dh || !json.keys?.auth) return false;
    const res = await fetch(`${HTTP_BASE}/api/push/subscribe`, {
      method: "POST",
      headers: { ...authHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify({
        endpoint: json.endpoint,
        keys: { p256dh: json.keys.p256dh, auth: json.keys.auth },
        user_agent: navigator.userAgent,
      }),
    });
    return res.ok;
  } catch {
    return false;
  }
}
