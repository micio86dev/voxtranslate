// VoxTranslate service worker — makes the app installable + offline-capable.
// Conservative caching: network-first for navigations (so the app stays fresh),
// cache-first for immutable hashed assets, and cross-origin requests (the
// Railway API/WebSocket) are left untouched.

const CACHE = 'voxtranslate-v2';
const SHELL = ['/', '/manifest.webmanifest', '/icon.png', '/icon-maskable.png'];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((c) => c.addAll(SHELL))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);

  // Let cross-origin requests (Railway /rooms, etc.) and WebSockets go straight
  // to the network — never cache backend responses.
  if (url.origin !== self.location.origin) return;

  // Navigations: network-first, fall back to the cached app shell when offline.
  if (req.mode === 'navigate') {
    event.respondWith(
      fetch(req)
        .then((res) => {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put('/', copy));
          return res;
        })
        .catch(() => caches.match('/')),
    );
    return;
  }

  // Content-hashed build assets are immutable — cache-first.
  if (url.pathname.startsWith('/_astro/')) {
    event.respondWith(
      caches.match(req).then(
        (cached) =>
          cached ||
          fetch(req).then((res) => {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(req, copy));
            return res;
          }),
      ),
    );
    return;
  }

  // Everything else same-origin: network, fall back to cache.
  event.respondWith(fetch(req).catch(() => caches.match(req)));
});

// --- Web Push (spec: scheduled meetings, Phase 1e) ---
// Payload from the server: { title, body, data:{ join_url, meeting_id, ... } }.
self.addEventListener('push', (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    payload = { title: 'VoxTranslate', body: event.data ? event.data.text() : '' };
  }
  const data = payload.data || {};
  event.waitUntil(
    self.registration.showNotification(payload.title || 'VoxTranslate', {
      body: payload.body || '',
      icon: '/icon.png',
      badge: '/icon.png',
      data,
    }),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const data = event.notification.data || {};
  const url = data.join_url || '/';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      for (const client of clients) {
        if ('focus' in client) {
          client.focus();
          if ('navigate' in client && url.startsWith('/')) client.navigate(url);
          return;
        }
      }
      return self.clients.openWindow(url);
    }),
  );
});
